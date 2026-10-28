// Naive recursive Fibonacci: about 2.7 million calls.
fn fib(n: i64) -> i64 { if n < 2 { return n; } fib(n - 1) + fib(n - 2) }
fn main() { println!("{}", fib(30)); }
