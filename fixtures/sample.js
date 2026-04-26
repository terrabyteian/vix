// Fixture: exercise javascript highlighting + symbol picker.

const MAX_RETRIES = 3;
const GREETING = "hello, world";

class Person {
    constructor(name, age) {
        this.name = name;
        this.age = age;
    }

    greet() {
        return `hi, ${this.name}! you are ${this.age}`;
    }
}

function double(x) {
    return x * 2;
}

const shout = (s) => `${s.toUpperCase()}!!!`;

async function fetchUser(id) {
    const res = await fetch(`/api/users/${id}`);
    if (!res.ok) throw new Error(`failed: ${res.status}`);
    return res.json();
}

function main() {
    const p = new Person("Ada", 36);
    const counts = new Map([["hits", 0]]);
    for (let i = 0; i < MAX_RETRIES; i++) {
        if (i % 2 === 0) {
            console.log(GREETING, "->", p.greet());
        } else {
            console.error(shout(GREETING));
        }
    }
    return double(21);
}

main();
