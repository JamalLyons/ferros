//! Trait objects for the value formatter integration test.

use std::fmt::Debug;
use std::hint::black_box;

#[derive(Debug)]
struct Reading
{
    count: u32,
}

trait Shape
{
    fn area(&self) -> u32;
}

struct Rectangle
{
    width: u32,
    height: u32,
}

struct Square
{
    side: u32,
}

impl Shape for Rectangle
{
    fn area(&self) -> u32
    {
        self.width * self.height
    }
}

impl Shape for Square
{
    fn area(&self) -> u32
    {
        self.side * self.side
    }
}

fn main()
{
    let reading = Reading { count: 11 };
    let debug: &dyn Debug = &reading;
    let rectangle: Box<dyn Shape> = Box::new(Rectangle { width: 3, height: 5 });
    let square: Box<dyn Shape> = Box::new(Square { side: 7 });
    black_box((&debug, &rectangle, &square)); // BREAK: inspect
    black_box((reading.count, rectangle.area(), square.area()));
}
