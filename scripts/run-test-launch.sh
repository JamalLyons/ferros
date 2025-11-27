sudo cargo build --example test_target
sudo cargo build --example simple_test

sudo cargo run -- --log-level debug launch target/debug/examples/test_target
sudo cargo run -- --log-level info launch target/debug/examples/simple_test