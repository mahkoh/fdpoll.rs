### fdpoll.rs

A library for integrating file descriptors into Rust's select pattern.

[Documentation](https://mahkoh.github.io/fdpoll/doc/fdpoll)

#### Example

```rust
let fdpoll = FDPoll::new(3).unwrap();
fdpoll.add(0, Read).unwrap();
fdpoll.wait().unwrap();
 
let select = Select::new();
let mut handle = select.handle(&fdpoll.rcv);
unsafe { handle.add() };

loop {
    let ret = select.wait();
    if ret == handle.id() {
        fdpoll.rcv.recv().unwrap();
        for e in fdpoll.events().unwrap().slice().iter() {
            println!("fd: {} read: {} write: {}", e.fd(), e.read(), e.write());
            if e.fd() == 0 {
                println!(":::{}", std::io::stdin().read_line().ok());
            }
        }
        fdpoll.wait().unwrap();
    }
}
```
