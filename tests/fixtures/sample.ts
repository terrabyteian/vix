export interface Greeter {
  greet(name: string): string;
}

export class FriendlyGreeter implements Greeter {
  constructor(private readonly prefix: string = "Hello") {}

  greet(name: string): string {
    return `${this.prefix}, ${name}!`;
  }
}

const greeter = new FriendlyGreeter();
const names = ["alice", "bob", "carol"];
for (const name of names) {
  console.log(greeter.greet(name));
}
