//! Консольное приложение для macOS: запуск JACK2, загрузка netmanager
//! и автоматическая коммутация 26 каналов CoreAudio -> StreamPC.
//!
//! Последовательность:
//! 1. `jackd -d coreaudio -d "PaeFireStudioAudioEngine:0" -r 48000 -p 512`
//!    в дочернем процессе, stdout/stderr читаются в отдельных потоках.
//! 2. Опрос JACK API с флагом `NO_START_SERVER`, пока сервер не начнёт
//!    принимать клиентские подключения (таймаут 20 с).
//! 3. Загрузка внутреннего клиента `netmanager` через JACK API,
//!    при неудаче — эквивалент `jack_load netmanager`.
//! 4. Соединения `system:capture_i` -> `StreamPC:to_slave_i` (i = 1..=26)
//!    с повторными попытками: порты StreamPC появляются с задержкой.
//!
//! Ctrl+C корректно останавливает порождённый jackd (Drop + kill).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use jack::{Client, ClientOptions, Error as JackError};

// ---------------------------------------------------------------------------
// Конфигурация
// ---------------------------------------------------------------------------

/// Имя CoreAudio-устройства для драйвера JACK.
const COREAUDIO_DEVICE: &str = "PaeFireStudioAudioEngine:0";
/// Частота дискретизации, Гц.
const SAMPLE_RATE: u32 = 48_000;
/// Размер периода (period / buffer) в сэмплах.
const PERIOD_SIZE: u32 = 512;
/// Имя внутреннего клиента JACK (совпадает с именем .dylib/.so).
const INTERNAL_CLIENT: &str = "netmanager";
/// Имя нашего JACK-клиента в графе.
const CLIENT_NAME: &str = "jack2-autoconnect";
/// Число каналов для автокоммутации (включительно).
const CHANNEL_COUNT: u32 = 26;
/// Шаблон порта-источника (физический capture CoreAudio).
const SRC_PORT_FMT: &str = "system:capture_{}";
/// Шаблон порта-приёмника (клиент StreamPC / netjack).
const DST_PORT_FMT: &str = "StreamPC:to_slave_{}";
/// Максимальное ожидание готовности jackd.
const JACK_READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Интервал опроса сокета JACK.
const JACK_READY_POLL: Duration = Duration::from_millis(250);
/// Максимальное ожидание портов StreamPC после загрузки netmanager.
const PORT_RETRY_TIMEOUT: Duration = Duration::from_secs(60);
/// Пауза между попытками connect.
const PORT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Короткая пауза, чтобы netmanager успел зарегистрировать клиент.
const AFTER_LOAD_PAUSE: Duration = Duration::from_millis(400);

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Ошибка: {err}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), AppError> {
    println!("=== JACK2 autoconnect (macOS / CoreAudio) ===");
    println!("Устройство : {COREAUDIO_DEVICE}");
    println!("Частота    : {SAMPLE_RATE} Hz");
    println!("Буфер      : {PERIOD_SIZE} frames");
    println!("Каналы     : 1..{CHANNEL_COUNT}");
    println!();

    // Демон jackd живёт как наш потомок. Без явного kill он останется
    // сиротой после Ctrl+C (на macOS launchd его не подберёт).
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            eprintln!("\nПолучен сигнал завершения, останавливаю JACK...");
            shutdown.store(true, Ordering::SeqCst);
        })
        .map_err(|e| AppError::Signal(e.to_string()))?;
    }

    // Повторный запуск утилиты не должен плодить второй jackd.
    let mut daemon: Option<JackDaemon> = None;
    if jack_is_ready() {
        println!("JACK уже запущен, повторный старт jackd не требуется.");
    } else {
        println!("Запускаю jackd...");
        daemon = Some(JackDaemon::spawn()?);
    }

    let client = wait_for_jack_ready(&shutdown, daemon.as_mut())?;
    println!(
        "Сервер JACK готов (sample_rate={}, buffer={}). Клиент «{}».",
        client.sample_rate(),
        client.buffer_size(),
        client.name()
    );

    load_internal_client(&client, INTERNAL_CLIENT)?;
    thread::sleep(AFTER_LOAD_PAUSE);

    connect_all_channels(&client, &shutdown)?;
    println!("Коммутация завершена: {CHANNEL_COUNT} пар портов.");
    println!("Приложение работает. Ctrl+C — остановить jackd и выйти.");

    supervise(&shutdown, daemon.as_mut())?;
    println!("Выход.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Запуск и сопровождение процесса jackd
// ---------------------------------------------------------------------------

/// Обёртка над дочерним `jackd`: логи в фоне, kill при Drop.
struct JackDaemon {
    child: Child,
}

impl JackDaemon {
    fn spawn() -> Result<Self, AppError> {
        let jackd = find_binary("jackd")?;
        println!("Бинарник jackd: {}", jackd.display());

        let mut command = Command::new(&jackd);
        command
            .arg("-d")
            .arg("coreaudio")
            .arg("-d")
            .arg(COREAUDIO_DEVICE)
            .arg("-r")
            .arg(SAMPLE_RATE.to_string())
            .arg("-p")
            .arg(PERIOD_SIZE.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        prepend_common_path(&mut command);

        let mut child = command.spawn().map_err(|e| AppError::Spawn {
            binary: jackd.display().to_string(),
            source: e.to_string(),
        })?;

        // Pipe должен постоянно опустошаться, иначе jackd зависнет на write.
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || pump_lines("jackd", stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || pump_lines("jackd", stderr));
        }

        Ok(Self { child })
    }

    fn poll(&mut self) -> Result<Option<ExitStatus>, AppError> {
        self.child
            .try_wait()
            .map_err(|e| AppError::Daemon(format!("не удалось опросить jackd: {e}")))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for JackDaemon {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(None) => {
                eprintln!(
                    "Останавливаю дочерний процесс jackd (pid {})...",
                    self.child.id()
                );
                self.kill();
            }
            _ => {}
        }
    }
}

fn pump_lines<R: std::io::Read>(prefix: &str, reader: R) {
    let reader = BufReader::new(reader);
    for line in reader.lines() {
        match line {
            Ok(text) => println!("[{prefix}] {text}"),
            Err(_) => break,
        }
    }
}

/// Homebrew (Intel/ARM) и JackOSX кладут утилиты вне стандартного PATH GUI-приложений.
fn prepend_common_path(command: &mut Command) {
    let extra = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/local/jack/bin",
        "/Applications/JACK2.app/Contents/MacOS",
    ];
    let mut path = extra.join(":");
    if let Ok(existing) = std::env::var("PATH") {
        path.push(':');
        path.push_str(&existing);
    }
    command.env("PATH", path);
}

fn find_binary(name: &str) -> Result<PathBuf, AppError> {
    let extra = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/local/jack/bin",
        "/Applications/JACK2.app/Contents/MacOS",
    ];

    if let Ok(path) = which(name) {
        return Ok(path);
    }
    for dir in extra {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(AppError::BinaryNotFound(name.to_string()))
}

fn which(name: &str) -> Result<PathBuf, ()> {
    let path = std::env::var("PATH").map_err(|_| ())?;
    for dir in path.split(':') {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

// ---------------------------------------------------------------------------
// Ожидание готовности JACK через API
// ---------------------------------------------------------------------------

fn jack_is_ready() -> bool {
    Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER).is_ok()
}

fn wait_for_jack_ready(
    shutdown: &AtomicBool,
    daemon: Option<&mut JackDaemon>,
) -> Result<Client, AppError> {
    let started = Instant::now();
    let mut daemon = daemon;
    let mut last_log = Instant::now() - Duration::from_secs(2);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Err(AppError::Interrupted);
        }

        // Демон мог упасть ещё на этапе открытия CoreAudio-устройства
        // (нет PaeFireStudio, занят sample rate, нет прав на HAL).
        if let Some(d) = daemon.as_mut() {
            if let Some(status) = d.poll()? {
                return Err(AppError::Daemon(format!(
                    "jackd завершился до готовности сервера (код {status})"
                )));
            }
        }

        match Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER) {
            Ok((client, status)) => {
                if !status.is_empty() {
                    println!("JACK ClientStatus: {status:?}");
                }
                return Ok(client);
            }
            Err(JackError::LibraryError(msg)) => {
                return Err(AppError::Daemon(format!(
                    "не удалось загрузить libjack: {msg}. \
                     Установите JACK2 (brew install jack) и проверьте DYLD_LIBRARY_PATH."
                )));
            }
            Err(err) => {
                if last_log.elapsed() >= Duration::from_secs(2) {
                    println!(
                        "ожидание JACK: {err} (прошло {:.1}s / лимит {:.0}s)",
                        started.elapsed().as_secs_f32(),
                        JACK_READY_TIMEOUT.as_secs_f32()
                    );
                    last_log = Instant::now();
                }
            }
        }

        if started.elapsed() >= JACK_READY_TIMEOUT {
            return Err(AppError::Timeout(format!(
                "сервер JACK не начал принимать подключения за {:?}",
                JACK_READY_TIMEOUT
            )));
        }

        thread::sleep(JACK_READY_POLL);
    }
}

// ---------------------------------------------------------------------------
// Загрузка внутреннего модуля (netmanager)
// ---------------------------------------------------------------------------

fn load_internal_client(client: &Client, name: &str) -> Result<(), AppError> {
    println!("Загружаю внутренний клиент «{name}»...");

    // Предпочтительный путь: JACK C-API jack_internal_client_load.
    // client_name = имя в графе, client_bin_name = имя .dylib без расширения.
    match client.load_internal_client(name, name, "") {
        Ok(id) => {
            println!("Модуль «{name}» загружен через JACK API (id={id}).");
            return Ok(());
        }
        Err(err) => {
            let msg = err.to_string().to_lowercase();
            if looks_already_loaded(&msg) {
                println!("Модуль «{name}» уже загружен, продолжаю.");
                return Ok(());
            }
            eprintln!("JACK API load_internal_client: {err}. Пробую jack_load...");
        }
    }

    load_internal_client_via_cli(name)
}

fn load_internal_client_via_cli(name: &str) -> Result<(), AppError> {
    let jack_load = find_binary("jack_load")?;
    println!("Вызов: {} {name}", jack_load.display());

    let mut command = Command::new(&jack_load);
    command.arg(name);
    prepend_common_path(&mut command);

    let output = command.output().map_err(|e| AppError::Spawn {
        binary: jack_load.display().to_string(),
        source: e.to_string(),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !stdout.trim().is_empty() {
        print!("{stdout}");
    }
    if !stderr.trim().is_empty() {
        eprint!("{stderr}");
    }

    if output.status.success() {
        println!("Модуль «{name}» загружен через jack_load.");
        return Ok(());
    }

    if looks_already_loaded(&combined.to_lowercase()) {
        println!("Модуль «{name}» уже загружен, продолжаю.");
        return Ok(());
    }

    Err(AppError::JackLoad {
        module: name.to_string(),
        status: output.status.to_string(),
        stderr: stderr.trim().to_string(),
    })
}

fn looks_already_loaded(msg: &str) -> bool {
    msg.contains("already") || msg.contains("exists") || msg.contains("loaded")
}

// ---------------------------------------------------------------------------
// Автокоммутация каналов с retry
// ---------------------------------------------------------------------------

fn connect_all_channels(client: &Client, shutdown: &AtomicBool) -> Result<(), AppError> {
    println!("Коммутация {CHANNEL_COUNT} каналов: system:capture_i -> StreamPC:to_slave_i");

    let started = Instant::now();
    let mut connected = vec![false; CHANNEL_COUNT as usize];
    let mut last_report = Instant::now();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Err(AppError::Interrupted);
        }

        let mut pending = 0u32;
        for i in 1..=CHANNEL_COUNT {
            let idx = (i - 1) as usize;
            if connected[idx] {
                continue;
            }

            let src = SRC_PORT_FMT.replace("{}", &i.to_string());
            let dst = DST_PORT_FMT.replace("{}", &i.to_string());

            match try_connect(client, &src, &dst) {
                ConnectResult::Done => {
                    println!("  OK  {src} -> {dst}");
                    connected[idx] = true;
                }
                ConnectResult::Retry(reason) => {
                    pending += 1;
                    if last_report.elapsed() >= Duration::from_secs(3) {
                        println!("  ... {src} -> {dst}: {reason}");
                    }
                }
                ConnectResult::Fatal(err) => return Err(err),
            }
        }

        if pending == 0 {
            return Ok(());
        }

        if started.elapsed() >= PORT_RETRY_TIMEOUT {
            let missing: Vec<String> = connected
                .iter()
                .enumerate()
                .filter(|(_, ok)| !**ok)
                .map(|(idx, _)| {
                    let i = idx as u32 + 1;
                    format!(
                        "{} -> {}",
                        SRC_PORT_FMT.replace("{}", &i.to_string()),
                        DST_PORT_FMT.replace("{}", &i.to_string())
                    )
                })
                .collect();
            return Err(AppError::Timeout(format!(
                "не удалось соединить {} пар портов за {:?}:\n  {}",
                missing.len(),
                PORT_RETRY_TIMEOUT,
                missing.join("\n  ")
            )));
        }

        if last_report.elapsed() >= Duration::from_secs(3) {
            println!(
                "Ожидаю порты StreamPC: осталось {pending}, прошло {:.1}s",
                started.elapsed().as_secs_f32()
            );
            last_report = Instant::now();
        }

        thread::sleep(PORT_RETRY_INTERVAL);
    }
}

enum ConnectResult {
    Done,
    Retry(String),
    Fatal(AppError),
}

fn try_connect(client: &Client, src: &str, dst: &str) -> ConnectResult {
    // StreamPC публикует порты только после установления netjack-сессии.
    let src_exists = client.port_by_name(src).is_some();
    let dst_exists = client.port_by_name(dst).is_some();

    if !src_exists || !dst_exists {
        let missing = match (src_exists, dst_exists) {
            (false, false) => format!("нет портов {src} и {dst}"),
            (false, true) => format!("нет порта {src}"),
            (true, false) => format!("нет порта {dst}"),
            (true, true) => unreachable!(),
        };
        return ConnectResult::Retry(missing);
    }

    if already_connected(client, src, dst) {
        return ConnectResult::Done;
    }

    match client.connect_ports_by_name(src, dst) {
        Ok(()) => ConnectResult::Done,
        Err(JackError::PortAlreadyConnected(_, _)) => ConnectResult::Done,
        Err(err) => {
            if is_transient(&err) {
                ConnectResult::Retry(err.to_string())
            } else {
                ConnectResult::Fatal(AppError::Connect {
                    src: src.to_string(),
                    dst: dst.to_string(),
                    source: err.to_string(),
                })
            }
        }
    }
}

fn already_connected(client: &Client, src: &str, dst: &str) -> bool {
    client
        .port_by_name(src)
        .and_then(|port| port.is_connected_to(dst).ok())
        .unwrap_or(false)
}

fn is_transient(err: &JackError) -> bool {
    match err {
        JackError::PortConnectionError { .. } => true,
        JackError::ClientIsNoLongerAlive => false,
        other => {
            let msg = other.to_string().to_lowercase();
            msg.contains("not found")
                || msg.contains("no such")
                || msg.contains("unknown")
                || msg.contains("unavailable")
                || msg.contains("try again")
        }
    }
}

// ---------------------------------------------------------------------------
// Надзор за демоном до Ctrl+C
// ---------------------------------------------------------------------------

fn supervise(shutdown: &AtomicBool, mut daemon: Option<&mut JackDaemon>) -> Result<(), AppError> {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(d) = daemon.as_mut() {
            if let Some(status) = d.poll()? {
                return Err(AppError::Daemon(format!(
                    "процесс jackd неожиданно завершился ({status})"
                )));
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// Ошибки
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum AppError {
    BinaryNotFound(String),
    Spawn { binary: String, source: String },
    Daemon(String),
    Timeout(String),
    JackLoad {
        module: String,
        status: String,
        stderr: String,
    },
    Connect {
        src: String,
        dst: String,
        source: String,
    },
    Signal(String),
    Interrupted,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::BinaryNotFound(name) => write!(
                f,
                "не найден исполняемый файл «{name}». Установите JACK2 и добавьте его в PATH \
                 (Homebrew: /opt/homebrew/bin, JackOSX: /usr/local/bin)."
            ),
            AppError::Spawn { binary, source } => {
                write!(f, "не удалось запустить {binary}: {source}")
            }
            AppError::Daemon(msg) => write!(f, "{msg}"),
            AppError::Timeout(msg) => write!(f, "таймаут: {msg}"),
            AppError::JackLoad {
                module,
                status,
                stderr,
            } => write!(
                f,
                "jack_load {module} завершился с {status}{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ),
            AppError::Connect { src, dst, source } => {
                write!(f, "не удалось соединить {src} -> {dst}: {source}")
            }
            AppError::Signal(msg) => {
                write!(f, "не удалось установить обработчик сигнала: {msg}")
            }
            AppError::Interrupted => write!(f, "прервано пользователем"),
        }
    }
}

impl std::error::Error for AppError {}
