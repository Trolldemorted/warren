//! Thin binary wrapper. All logic lives in the `rabbit-lib` library crate;
//! see `rabbit-lib/src/lib.rs::run`.

fn main() -> anyhow::Result<()> {
    rabbit_lib::run()
}
