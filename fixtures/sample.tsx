// Fixture: exercise tsx highlighting + symbol picker.

import { useState, useEffect } from "react";

interface Props {
    name: string;
    initialCount?: number;
}

type Status = "idle" | "loading" | "ready";

export function Counter({ name, initialCount = 0 }: Props) {
    const [count, setCount] = useState<number>(initialCount);
    const [status, setStatus] = useState<Status>("idle");

    useEffect(() => {
        setStatus("loading");
        const t = setTimeout(() => setStatus("ready"), 200);
        return () => clearTimeout(t);
    }, [name]);

    return (
        <div className="counter" data-status={status}>
            <h1>hello, {name}</h1>
            <p>count: {count}</p>
            <button onClick={() => setCount((c) => c + 1)}>
                {status === "ready" ? "increment" : "..."}
            </button>
            {count > 5 && <em>you clicked a lot</em>}
        </div>
    );
}

export default function App() {
    return <Counter name="Ada" initialCount={3} />;
}
