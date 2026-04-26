//! Fixture: exercise rust highlighting + symbol picker.

use std::collections::HashMap;
use std::fmt;

const MAX_RETRIES: u32 = 3;
static GREETING: &str = "hello, world";

pub trait Greet {
    fn greet(&self) -> String;
}

#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
    pub age: u32,
}

impl Greet for Person {
    fn greet(&self) -> String {
        format!("hi, {}! you are {}", self.name, self.age)
    }
}

pub enum Status {
    Online,
    Offline,
    Away(String),
}

macro_rules! shout {
    ($s:expr) => { format!("{}!!!", $s.to_uppercase()) };
}

mod helpers {
    pub fn double(x: i64) -> i64 { x * 2 }
}

fn main() {
    let p = Person { name: "Ada".into(), age: 36 };
    let mut counts: HashMap<&str, u32> = HashMap::new();
    counts.insert("hits", 0);
    for i in 0..MAX_RETRIES {
        if i % 2 == 0 {
            println!("{} -> {}", GREETING, p.greet());
        } else {
            eprintln!("{}", shout!(GREETING));
        }
    }
    let _ = helpers::double(21);
}
