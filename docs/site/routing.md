# Pools, keys, routing, and overage

A pool holds one or more accounts, each with a priority; a key routes into exactly one pool; every request a key
presents is placed on one account of that pool by a selection that honours the session's pin, the pool's strategy,
and each account's quota. This page describes every field of an account, a pool, and a key, then the selection
itself, and the answers a client gets when nothing can serve it.
