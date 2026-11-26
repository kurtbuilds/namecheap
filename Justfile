set next

# Install the `namecheap` binary into ~/.cargo/bin
install:
    cargo install --path .

# Build the project
build:
    cargo build --release

# Run the CLI, e.g. `just run list`
run *ARGS:
    cargo run -- $ARGS

# Run tests
test:
    cargo test
