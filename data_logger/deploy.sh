cargo build --release --target aarch64-unknown-linux-gnu
scp /tmp/rust_target/aarch64-unknown-linux-gnu/release//tokio SERVER:/opt/meteo/