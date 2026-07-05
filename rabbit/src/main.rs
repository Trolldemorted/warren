//! Thin binary wrapper. All logic lives in the `rabbit` library crate so it is
//! reachable from `tests/` and downstream embedders; see `src/lib.rs`.

fn main() -> anyhow::Result<()> {
    rabbit::run()
}
