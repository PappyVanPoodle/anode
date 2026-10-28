// Sieve of Eratosthenes: counts the primes up to 2,000,000.
fn count_primes(limit: usize) -> i64 {
    let mut composite: Vec<i64> = Vec::new();
    for _ in 0..=limit { composite.push(0); }
    let mut count = 0;
    for p in 2..=limit {
        if composite[p] == 0 {
            count += 1;
            let mut m = p * p;
            while m <= limit { composite[m] = 1; m += p; }
        }
    }
    count
}
fn main() { println!("{}", count_primes(2000000)); }
