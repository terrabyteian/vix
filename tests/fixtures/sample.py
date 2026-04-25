"""Toy module used as a fixture for editor tests."""

from dataclasses import dataclass


@dataclass
class Counter:
    label: str
    count: int = 0

    def tick(self) -> int:
        self.count += 1
        return self.count


def greet(name: str) -> str:
    return f"Hello, {name}!"


def main() -> None:
    counter = Counter("greetings")
    for name in ["alice", "bob", "carol"]:
        print(greet(name))
        counter.tick()


if __name__ == "__main__":
    main()
