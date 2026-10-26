/// Adds one to the input.
pub fn add_one(input: u32) -> u32 {
    input + 1
}

pub fn caller() -> u32 {
    add_one(41)
}
