#[path = "../../crates/node/src/version/stamped.rs"]
mod stamped;

fn main() {
    println!("{}", stamped::revision());
}
