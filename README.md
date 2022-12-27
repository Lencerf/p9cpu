# BUILD
```
RUSTFLAGS='-C target-feature=+crt-static' cargo build --target x86_64-unknown-linux-gnu --release
```
this will generate the client `target/x86_64-unknown-linux-gnu/release/p9cpu` and
the server `target/x86_64-unknown-linux-gnu/release/p9cpud`.

Checkout usage by `p9cpu --help` and `p9cpud --help`.

# Example

Run the server by:
```
./p9cpud --net vsock --port 12346
```

Run the client by:
```
PWD=/ ./p9cpu --tty --net vsock --port 12346 --namespace /lib:/lib64:/usr:/bin:/etc:/home:/export $CID -- /bbin/elvish
```