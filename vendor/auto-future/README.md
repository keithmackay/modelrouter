<div align="center">
  <h1>
    Auto Future<br>
    a means for easily building futurable structs
  </h1>

  [![crate](https://img.shields.io/crates/v/auto-future.svg)](https://crates.io/crates/auto-future)
  [![docs](https://docs.rs/auto-future/badge.svg)](https://docs.rs/auto-future)
</div>

This is for quickly making structs futurable, where the future implementation is an underlying `async fn`.

See this example for details ...

```rust
  use ::auto_future::AutoFuture;

  struct ExampleStruct;

  impl ExampleStruct {
    async fn do_async_work(self) -> u32 {
      // perform a bunch of awaited calls ...

      123
    }
  }

  impl IntoFuture for ExampleStruct {
      type Output = u32;
      type IntoFuture = AutoFuture<u32>;

      fn into_future(self) -> Self::IntoFuture {
          let raw_future = self.do_async_work();
          AutoFuture::new(raw_future)
      }
  }
```
