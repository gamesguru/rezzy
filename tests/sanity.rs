use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Debug, PartialEq, Eq)]
struct Item(i32);

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher value is Smaller = Pops Last
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[test]
fn test_heap_order() {
    let mut heap = BinaryHeap::new();
    heap.push(Item(100));
    heap.push(Item(50));

    let first = heap.pop().unwrap();
    let second = heap.pop().unwrap();

    println!("First popped: {:?}", first);
    println!("Second popped: {:?}", second);

    assert_eq!(first.0, 50);
    assert_eq!(second.0, 100);
}
