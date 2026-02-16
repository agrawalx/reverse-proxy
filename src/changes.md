changes made after running cargo miri: 

replaced let node_ptr(): *mut LruEntry<K,V> = &mut *Node with Box::into_raw(node) as I derived the raw pointer from the mutable pointer and then later dropped the mutable pointer using mem::forget(). this was fine memory wise but we ended up invalidating the borrow stack since the owner of the raw pointer no longer existed 


changed node.as_mut() to *node_ptr
doing .as_mut() creates a mutable borrow which invalidates the previous borrows and then those borrows are used again while doing get which causes defined behavior