//! Ring buffers, linked lists, binary heaps and B-tree sets for the formatter
//! integration test.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, LinkedList, VecDeque};
use std::hint::black_box;

fn main()
{
    // Logical order starts at the physical head and wraps to slot 0.
    let mut wrapped = VecDeque::with_capacity(4);
    wrapped.extend([10_u32, 20, 30]);
    wrapped.pop_front();
    wrapped.push_back(40);
    wrapped.push_back(50);
    let mut pushed_front = VecDeque::with_capacity(4);
    pushed_front.extend([2_u32, 3]);
    pushed_front.push_front(1);
    let empty_deque = VecDeque::<u32>::new();
    let mut long_deque = VecDeque::with_capacity(20);
    long_deque.extend(0_u32..20);

    let list: LinkedList<char> = ['a', 'b', 'c'].into_iter().collect();
    let empty_list = LinkedList::<u8>::new();
    let long_list: LinkedList<u32> = (0..20).collect();

    let mut heap = BinaryHeap::with_capacity(4);
    heap.extend([1_u32, 3, 2]);
    let empty_heap = BinaryHeap::<u32>::new();

    let set: BTreeSet<u32> = [6, 5].into();
    let empty_set = BTreeSet::<u32>::new();
    let map: BTreeMap<u32, u8> = [(5, 50), (6, 60)].into();

    black_box(&wrapped); // BREAK: inspect
    black_box((&pushed_front, &empty_deque, &long_deque, &list, &empty_list, &long_list));
    black_box((&heap, &empty_heap, &set, &empty_set, &map));
}
