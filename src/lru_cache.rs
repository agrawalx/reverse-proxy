
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::ptr;
use std::mem; 
use std::mem::MaybeUninit;
use std::time::{Duration, Instant};

struct KeyRef<K> { k: *const K }

struct LruEntry<K, V>
{
    next: *mut LruEntry<K, V>,
    prev: *mut LruEntry<K, V>,
    key: MaybeUninit<K>,
    value: MaybeUninit<V>,
    expiry: Instant,
}

pub struct LruCache<K, V> {
    map: HashMap<KeyRef<K>, NonNull<LruEntry<K, V>>>,
    max_size: NonZeroUsize,
    head: *mut LruEntry<K, V>,
    tail: *mut LruEntry<K, V>,
}

impl<K: Hash> Hash for KeyRef<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe { (*self.k).hash(state) } // feed the value of pointer to the state not the address
    }
}

impl<K: PartialEq> PartialEq for KeyRef<K> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { *self.k == *other.k } // compare the values not address so we can't just derive these traits
    }
}

impl<K: Eq> Eq for KeyRef<K> {}

impl<K, V> LruEntry<K, V> {
    fn new(key: K, val: V, ttl: Duration) -> Self {
        LruEntry {
            key: mem::MaybeUninit::new(key),
            value: mem::MaybeUninit::new(val),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
            expiry: Instant::now() + ttl,
        }
    }

    fn new_sigil() -> Self {
        LruEntry {
            key: mem::MaybeUninit::uninit(),
            value: mem::MaybeUninit::uninit(),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
            expiry: Instant::now() + Duration::from_secs(u64::MAX / 2), // Far future
        }
    }
}

impl <K: Hash + Eq,V> LruCache<K,V> {
    pub fn new(capacity: NonZeroUsize) -> Self {
        let cache = LruCache {
            map: HashMap::with_capacity(capacity.get()),
            max_size : capacity,
            head: Box::into_raw(Box::new(LruEntry::new_sigil())),
            tail: Box::into_raw(Box::new(LruEntry::new_sigil()))
        };

        unsafe {
            (*cache.head).next = cache.tail;
            (*cache.tail).prev = cache.head;
        };

        cache

    }

    pub fn put(&mut self, k: K, v: V, ttl: Duration) {
        match self.map.get_mut(&KeyRef{k: &k}) {
            // case where node already exists
            Some(node) => {
                let node_ptr: *mut LruEntry<K, V> = node.as_ptr();
                unsafe {
                    // need to drop the value before assigning new value otherwise we have memory leak
                    (*node.as_mut()).value.assume_init_drop();
                    (*node.as_mut()).value = MaybeUninit::new(v);
                    (*node.as_mut()).expiry = Instant::now() + ttl;
                }
                self.detach(node_ptr);
                self.attach(node_ptr)

            }
            // node does not exist
            None => {
                let mut node = Box::new(LruEntry::new(k, v, ttl));
                // reference to the node 
                let node_ptr: *mut LruEntry<K,V> = &mut *node ;
                self.attach(node_ptr);
                let key_ptr = unsafe {(*node_ptr).key.as_ptr()}; 

                self.map.insert(
                    KeyRef { k: key_ptr },
                    NonNull::new(node_ptr).unwrap()
                );
                // without this, box gets dropped and our node_ptr becomes a dangling pointer
                mem::forget(node);

                if self.len() > self.capacity() {
                    self.remove_lru();
                }
            }
        };
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    fn capacity(&self) -> usize {
        self.max_size.get()
    }
    fn detach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            (*(*node).prev).next = (*node).next;
            (*(*node).next).prev = (*node).prev;
        }
    }

    fn attach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            (*node).next = (*self.head).next;
            (*node).prev = self.head;
            (*self.head).next = node;
            (*(*node).next).prev = node;
        }
    }

    fn remove_lru(&mut self) {
        unsafe {
            let lru = {(*self.tail).prev} ;
            if lru == self.head { return; }
            self.detach(lru); // removing the pointer to the node thats getting dropped
            let key_ptr = (*lru).key.as_ptr();
            self.map.remove(&KeyRef { k: key_ptr }); // removing the pointer from hashmap
            drop(Box::from_raw(lru)); // no pointer to the node exists so we can drop it now. 
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        // lifetime of returned reference to value is tied to &mut self 
        // we cant use .map method of option since self.map.get_mut mutably borrows self and then detach/attach also mutably borrows self and we can't have multiple mutable borrows
        let node_ptr = match self.map.get_mut(&KeyRef { k: key }) {
        Some(node) => node.as_ptr(),
        None => return None,
        };
        // borrow of map ends here so we can again have mutable borrows to self
        unsafe {
            // Check if entry has expired
            if Instant::now() >= (*node_ptr).expiry {
                // Remove expired entry
                self.detach(node_ptr);
                let key_ptr = (*node_ptr).key.as_ptr();
                self.map.remove(&KeyRef { k: key_ptr });
                drop(Box::from_raw(node_ptr));
                return None;
            }
            
            self.detach(node_ptr);
            self.attach(node_ptr);

            Some((*node_ptr).value.assume_init_ref())
        }
    }

    /// Remove all expired entries from the cache
    pub fn evict_expired(&mut self) -> usize {
        let now = Instant::now();
        let mut expired = Vec::new();
        
        unsafe {
            // Walk the list from tail (oldest) to head
            let mut current = (*self.tail).prev;
            while current != self.head {
                if now >= (*current).expiry {
                    expired.push(current);
                }
                current = (*current).prev;
            }
            
            // Remove all expired entries
            for node_ptr in expired.iter() {
                self.detach(*node_ptr);
                let key_ptr = (**node_ptr).key.as_ptr();
                self.map.remove(&KeyRef { k: key_ptr });
                drop(Box::from_raw(*node_ptr));
            }
        }
        
        expired.len()
    }
}

// need custom drop since we are using maybeUninit and also raw pointers are not dropped automatically so we will later face memory issues
impl<K, V> Drop for LruCache<K, V> {
    fn drop(&mut self) {
        unsafe {
            // Drain map = authoritative owner of nodes
            // walk the hashmap instead of list because list also contains the sentinel nodes
            self.map.drain().for_each(|(_, node)| {
                let node_ptr = node.as_ptr();

                // Reclaim allocation WITHOUT running Drop
                let mut entry = *Box::from_raw(node_ptr);

                // Manually drop ONLY initialized fields
                ptr::drop_in_place(entry.key.as_mut_ptr());
                ptr::drop_in_place(entry.value.as_mut_ptr());
            });

            // Free sentinel nodes WITHOUT dropping fields
            let _ = *Box::from_raw(self.head);
            let _ = *Box::from_raw(self.tail);
        }
    }
}

