"""Fixture: exercise python highlighting + symbol picker."""

from dataclasses import dataclass
from typing import Iterable

MAX_RETRIES = 3
GREETING = "hello, world"


@dataclass
class Person:
    name: str
    age: int

    def greet(self) -> str:
        return f"hi, {self.name}! you are {self.age}"


class Greeter:
    def __init__(self, prefix: str = "yo"):
        self.prefix = prefix

    def greet_all(self, people: Iterable[Person]) -> list[str]:
        return [f"{self.prefix} {p.greet()}" for p in people]


def double(x: int) -> int:
    return x * 2


def main() -> None:
    p = Person(name="Ada", age=36)
    g = Greeter()
    for i in range(MAX_RETRIES):
        if i % 2 == 0:
            print(GREETING, "->", p.greet())
        else:
            print(GREETING.upper() + "!!!")
    print(g.greet_all([p, Person("Linus", 54)]))
    assert double(21) == 42


if __name__ == "__main__":
    main()
