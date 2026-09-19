# Keepalive

A prompt cache entry expires five minutes, or one hour, after its last use. With keepalive enabled on a pool, the
router keeps a replayable copy of every eligible request a session sends and, shortly before the entry would
expire, sends a probe that reads it. This page covers how the engine learns which sessions are alive, when a probe
may run, every tuning field, and how the ledger accounts what a probe cost against what it saved.
