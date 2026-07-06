//! Thin binary wrapper. All logic lives in this crate's library;
//! see `rabbit::run`.

fn main() -> anyhow::Result<()> {
    rabbit::run()
}
