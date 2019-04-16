#![allow(dead_code)]

fn invocate_func(a: &str, b: &str, c: &str) -> String {
    format!("{} {} {}", a, b, c)
}

#[test]
fn test_it_works() {
    let y = invocate_func(
        "first",
        "second",
        "third",
    );

    assert_eq!(y.as_str(), "first second third");
}
