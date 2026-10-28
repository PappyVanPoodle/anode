// Counts 300,000 words drawn from a 5,000-word vocabulary with a hash map,
// then reports the vocabulary size, the most frequent count and a checksum.
use std::collections::HashMap;
fn main() {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for i in 0..300000i64 {
        let word = format!("w{}", i * 7919 % 5000);
        *counts.entry(word).or_insert(0) += 1;
    }
    let mut most = 0;
    let mut checksum = 0;
    for (name, &seen) in &counts {
        if seen > most { most = seen; }
        checksum += seen * name.len() as i64;
    }
    println!("{}\n{}\n{}", counts.len(), most, checksum);
}
