fn main() {
    let names = ["Mittens", "Jingle Paws", "Sir Fluffy"];
    let placeholders = names.iter().map(|_| "(?)").collect::<Vec<_>>().join(", ");
    let q = format!("INSERT INTO cats (name) VALUES {}", placeholders);
    println!("{}", q);
    println!("{}", 12312313)
}
