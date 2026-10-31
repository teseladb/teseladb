--------------------------------- MODULE Reshard ---------------------------------
(* Resharding a collection online (018 FR-002): reshardCollection and refineCollectionShardKey *)
(* rewrite the collection under a new key order while clients keep writing.                    *)
(* Group 0 drives the phases. Start records the clone timestamp: the copy reads the old order   *)
(* at that MVCC snapshot (snap) and writes it key by key into the new order. Every write that   *)
(* the old order applies after the clone timestamp is also appended to the change stream of    *)
(* the operation (log, derived from the MVCC versions), which the new order applies in commit   *)
(* order once the copy is complete (the double write). EnterCritical starts the cutover: the    *)
(* donor ranges first refuse writes (blockedW; clients retry) and, once the new order has       *)
(* applied the whole stream, refuse reads too (blockedR); Commit then switches the collection  *)
(* in Group 0 atomically. Routers follow their copy of Group 0, which may lag: a stale router   *)
(* that reaches a blocked donor is refused and retries after refreshing. Abort (before Commit)  *)
(* drops the new order and unblocks the donors. Writes are sequential, each one acknowledged    *)
(* once applied, so the last acknowledged value is the one every reader must see.              *)
(* Invariants: the old order serves the acknowledged values while it serves reads, the new     *)
(* order serves them once committed (no write is lost at the cutover), and writes only stop    *)
(* during the cutover.                                                                          *)
EXTENDS Naturals, Sequences

CONSTANTS Key, Val, MaxWrites, MaxAttempts

None == 0

Running == {"clone", "apply", "critical"}

VARIABLES
    phase,      \* Group 0: "idle", "clone", "apply", "critical", "committed" or "aborted"
    attempts,   \* resharding operations started
    old,        \* the collection in its current key order: Key -> value (None: absent)
    new,        \* the collection in the new key order
    snap,       \* the old order at the clone timestamp
    copied,     \* keys the copy has written into the new order
    log,        \* writes applied to the old order after the clone timestamp, in commit order
    applied,    \* entries of log the new order has applied
    blockedW,   \* the donors refuse writes
    blockedR,   \* the donors refuse reads
    rview,      \* the router's copy of Group 0: "old" or "new"
    acked,      \* last acknowledged value per key
    writes      \* writes acknowledged

vars == <<phase, attempts, old, new, snap, copied, log, applied, blockedW, blockedR, rview,
          acked, writes>>

Empty == [k \in Key |-> None]

Init ==
    /\ phase = "idle" /\ attempts = 0
    /\ old = Empty /\ new = Empty /\ snap = Empty /\ copied = {}
    /\ log = <<>> /\ applied = 0
    /\ blockedW = FALSE /\ blockedR = FALSE
    /\ rview = "old" /\ acked = Empty /\ writes = 0

\* reshardCollection: Group 0 records the operation and its clone timestamp.
Start ==
    /\ phase \in {"idle", "aborted"} /\ attempts < MaxAttempts
    /\ phase' = "clone" /\ attempts' = attempts + 1
    /\ snap' = old /\ new' = Empty /\ copied' = {} /\ log' = <<>> /\ applied' = 0
    /\ UNCHANGED <<old, blockedW, blockedR, rview, acked, writes>>

\* The copy writes one key of the snapshot into the new order.
CopyKey(k) ==
    /\ phase = "clone" /\ k \notin copied
    /\ new' = [new EXCEPT ![k] = snap[k]]
    /\ copied' = copied \cup {k}
    /\ UNCHANGED <<phase, attempts, old, snap, log, applied, blockedW, blockedR, rview, acked,
                   writes>>

CloneDone ==
    /\ phase = "clone" /\ copied = Key
    /\ phase' = "apply"
    /\ UNCHANGED <<attempts, old, new, snap, copied, log, applied, blockedW, blockedR, rview,
                   acked, writes>>

\* The new order applies the next entry of the change stream.
ApplyNext ==
    /\ phase \in {"apply", "critical"} /\ applied < Len(log)
    /\ LET e == log[applied + 1] IN new' = [new EXCEPT ![e[1]] = e[2]]
    /\ applied' = applied + 1
    /\ UNCHANGED <<phase, attempts, old, snap, copied, log, blockedW, blockedR, rview, acked,
                   writes>>

EnterCritical ==
    /\ phase = "apply"
    /\ phase' = "critical"
    /\ UNCHANGED <<attempts, old, new, snap, copied, log, applied, blockedW, blockedR, rview,
                   acked, writes>>

\* The donors apply the critical section's entry in their range logs.
BlockWrites ==
    /\ phase = "critical" /\ ~blockedW
    /\ blockedW' = TRUE
    /\ UNCHANGED <<phase, attempts, old, new, snap, copied, log, applied, blockedR, rview, acked,
                   writes>>

\* With writes stopped and the stream applied, the donors stop reads for the switch.
BlockReads ==
    /\ phase = "critical" /\ blockedW /\ ~blockedR /\ applied = Len(log)
    /\ blockedR' = TRUE
    /\ UNCHANGED <<phase, attempts, old, new, snap, copied, log, applied, blockedW, rview, acked,
                   writes>>

\* commitReshardCollection, or the automatic commit: Group 0 switches the collection.
Commit ==
    /\ phase = "critical" /\ blockedW /\ blockedR /\ applied = Len(log)
    /\ phase' = "committed"
    /\ UNCHANGED <<attempts, old, new, snap, copied, log, applied, blockedW, blockedR, rview,
                   acked, writes>>

\* abortReshardCollection, a cancellation or a failure before the decision.
Abort ==
    /\ phase \in Running
    /\ phase' = "aborted"
    /\ new' = Empty /\ blockedW' = FALSE /\ blockedR' = FALSE
    /\ UNCHANGED <<attempts, old, snap, copied, log, applied, rview, acked, writes>>

Refresh ==
    /\ rview' = IF phase = "committed" THEN "new" ELSE "old"
    /\ rview' /= rview
    /\ UNCHANGED <<phase, attempts, old, new, snap, copied, log, applied, blockedW, blockedR,
                   acked, writes>>

\* A client write through the router. A blocked donor refuses it: the client retries later.
Write(k, v) ==
    /\ writes < MaxWrites
    /\ IF rview = "old"
       THEN /\ ~blockedW
            /\ old' = [old EXCEPT ![k] = v]
            /\ log' = IF phase \in Running THEN Append(log, <<k, v>>) ELSE log
            /\ UNCHANGED new
       ELSE /\ new' = [new EXCEPT ![k] = v]
            /\ UNCHANGED <<old, log>>
    /\ acked' = [acked EXCEPT ![k] = v]
    /\ writes' = writes + 1
    /\ UNCHANGED <<phase, attempts, snap, copied, applied, blockedW, blockedR, rview>>

Next ==
    \/ \E k \in Key, v \in Val : Write(k, v)
    \/ \E k \in Key : CopyKey(k)
    \/ Start \/ CloneDone \/ ApplyNext \/ EnterCritical \/ BlockWrites \/ BlockReads \/ Commit
    \/ Abort \/ Refresh

Spec == Init /\ [][Next]_vars

\* A router whose copy still shows the old order reads it unless the donors refuse reads.
OldServesAcked == ~blockedR => old = acked

\* No write is lost at the cutover: once committed, the new order has every acknowledged value.
NewServesAcked == phase = "committed" => new = acked

\* Writes only stop during the cutover (the critical section and after the switch).
WritesStopOnlyAtCutover == blockedW => phase \in {"critical", "committed"}
=============================================================================
