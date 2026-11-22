fn main() {
    let mode = std::env::args().nth(1).expect("numeric or pipeline");
    let value: i64 = if mode == "numeric" {
        (0..5_000_000_i64).map(|i| (i * 3) % 97).sum()
    } else {
        let mut values = [2, 3, 5, 7];
        (0..1_000_000_i64)
            .map(|i| {
                let index = (i % 4) as usize;
                let value = ((values[index] + i) * 7 + 3) % 997;
                values[index] = value;
                value
            })
            .sum()
    };
    println!("{value}");
}
