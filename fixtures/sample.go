// Fixture: exercise go highlighting + symbol picker.

package main

import (
	"errors"
	"fmt"
	"strings"
)

const (
	MaxRetries = 3
	Greeting   = "hello, world"
)

type Status int

const (
	Online Status = iota
	Offline
	Away
)

type Greeter interface {
	Greet() string
}

type Person struct {
	Name string
	Age  int
}

func (p Person) Greet() string {
	return fmt.Sprintf("hi, %s! you are %d", p.Name, p.Age)
}

func double(x int) int {
	return x * 2
}

func shout(s string) (string, error) {
	if s == "" {
		return "", errors.New("empty input")
	}
	return strings.ToUpper(s) + "!!!", nil
}

func main() {
	p := Person{Name: "Ada", Age: 36}
	for i := 0; i < MaxRetries; i++ {
		if i%2 == 0 {
			fmt.Println(Greeting, "->", p.Greet())
		} else if msg, err := shout(Greeting); err == nil {
			fmt.Println(msg)
		}
	}
	_ = double(21)
}
