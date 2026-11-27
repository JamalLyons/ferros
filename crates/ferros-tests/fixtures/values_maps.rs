//! Hash maps, sets and B-trees for the formatter integration test.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hint::black_box;

fn main()
{
    let mut hash = HashMap::with_capacity(32);
    hash.insert("alpha".to_owned(), 1_u32);
    hash.insert("café".to_owned(), 2);
    hash.insert("removed".to_owned(), 3);
    hash.insert("gamma".to_owned(), 4);
    hash.remove("removed");
    let empty_hash = HashMap::<String, u32>::new();
    let wide_hash: HashMap<u32, u32> = (0..40).map(|n| (n, n + 100)).collect();
    let set: HashSet<u32> = [5, 8, 13].into();
    let unit_set: HashSet<()> = [()].into();

    let tree: BTreeMap<u32, &str> = (0..15).rev().map(|n| (n, if n % 2 == 0 { "even" } else { "odd" })).collect();
    let empty_tree = BTreeMap::<u32, &str>::new();
    let wide_tree: BTreeMap<u32, &str> = (0..256).rev().map(|n| (n, "item")).collect();

    black_box(&hash); // BREAK: inspect
    black_box((&empty_hash, &wide_hash, &set, &unit_set, &tree, &empty_tree, &wide_tree));
}
