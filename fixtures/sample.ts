// Fixture: exercise typescript highlighting + symbol picker.

const MAX_RETRIES: number = 3;
const GREETING: string = "hello, world";

interface Greeter {
    greet(): string;
}

type Status = "online" | "offline" | { away: string };

class Person implements Greeter {
    constructor(public name: string, public age: number) {}

    greet(): string {
        return `hi, ${this.name}! you are ${this.age}`;
    }
}

function double(x: number): number {
    return x * 2;
}

async function fetchUser<T>(id: number): Promise<T> {
    const res = await fetch(`/api/users/${id}`);
    if (!res.ok) throw new Error(`failed: ${res.status}`);
    return res.json() as Promise<T>;
}

function main(): number {
    const p: Person = new Person("Ada", 36);
    const status: Status = { away: "lunch" };
    for (let i = 0; i < MAX_RETRIES; i++) {
        if (i % 2 === 0) {
            console.log(GREETING, "->", p.greet());
        }
    }
    return double(21);
}

main();
