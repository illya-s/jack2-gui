# jack2-gui

Консольное приложение для macOS: запуск JACK2 (CoreAudio), загрузка `netmanager` и автокоммутация 26 каналов.

```
system:capture_i  ->  StreamPC:to_slave_i    (i = 1..=26)
```

## Требования

- macOS
- JACK2 (`brew install jack` или JackOSX)
- Rust / Cargo
- Устройство CoreAudio `PaeFireStudioAudioEngine:0`

## Сборка и запуск

```
cargo build --release
cargo run --release
```

Бинарник: `target/release/jack2-gui`

Параметры jackd зашиты в `src/main.rs`:

```
jackd -d coreaudio -d "PaeFireStudioAudioEngine:0" -r 48000 -p 512
```

Ctrl+C останавливает порождённый процесс jackd.
