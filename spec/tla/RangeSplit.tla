------------------------------- MODULE RangeSplit -------------------------------
(* Splitting range G by prefix P into G and G' (001 FR-015).                                    *)
(* Each range is a linearizable state machine (its Raft log). Group 0 first records the intent  *)
(* (SplitRange, Pending); G's leader then proposes SetRangeBounds { out: [P] }: from the index   *)
(* i where it is applied, G answers OutOfRange to keys under P, and every replica builds G''s    *)
(* complete state at i (the keys under P and G's whole dedup table, which carries no keys) as   *)
(* a pending snapshot. CompleteSplit in Group 0 creates G' from that snapshot. Routers follow   *)
(* their copy of Group 0, which may lag: a stale one sends a key under P to G and gets          *)
(* OutOfRange, then retries in G' once its copy shows it, with the same request id. Answers may *)
(* be lost (the transaction applied but the client retries; its dedup entry answers the retry).*)
(* A garbage collection (GcStart)                                                               *)
(* and a split exclude each other in Group 0. A replica set change (SetReplicaEpoch) may land   *)
(* in G before or after i: G' starts with G's epoch at i and catches up like any range.         *)
(* Invariants: no acknowledged key is lost or served by the wrong range, no request is applied  *)
(* twice, a split and a collection never overlap, and a write is only applied with its range's  *)
(* current replica epoch.                                                                        *)
EXTENDS Naturals, FiniteSets

CONSTANTS Key, Under, Req, Val

None == 0

VARIABLES
    pending,    \* Group 0: SplitRange recorded and not completed
    done,       \* Group 0: CompleteSplit applied (G' exists)
    gc,         \* Group 0: a collection holds the partition (GcStart .. GcFinish)
    out,        \* G applied SetRangeBounds { out: [P] }
    gKeys,      \* G's keys: Key -> value (None: absent)
    gDedup,     \* G's dedup table: request ids applied
    snap,       \* G''s pending snapshot (built at i), or a record with ok = FALSE
    g2Keys,     \* G''s keys
    g2Dedup,    \* G''s dedup table
    fresh,      \* the router's copy of Group 0 shows G'
    reqs,       \* outstanding requests: id -> <<key, value>> (absent ids: not issued or answered)
    issued,     \* request ids used
    acked,      \* last acknowledged value per key
    applied,    \* times each request id was applied
    epoch0,     \* Group 0's replica epoch of the partition
    gEpoch,     \* G's replica epoch
    g2Epoch,    \* G''s replica epoch
    stale       \* a write was applied with an epoch that is not its range's

vars == <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs, issued,
          acked, applied, epoch0, gEpoch, g2Epoch, stale>>

NoSnap == [ok |-> FALSE, keys |-> [k \in Key |-> None], dedup |-> {}, epoch |-> 0]

Init ==
    /\ pending = FALSE /\ done = FALSE /\ gc = FALSE /\ out = FALSE
    /\ gKeys = [k \in Key |-> None] /\ gDedup = {}
    /\ snap = NoSnap
    /\ g2Keys = [k \in Key |-> None] /\ g2Dedup = {}
    /\ fresh = FALSE
    /\ reqs = [id \in {} |-> <<>>] /\ issued = {}
    /\ acked = [k \in Key |-> None]
    /\ applied = [id \in Req |-> 0]
    /\ epoch0 = 1 /\ gEpoch = 1 /\ g2Epoch = 1 /\ stale = FALSE

\* The client issues a write of `v` to `k` under a new id once its previous one was answered
\* (writes are sequential, so the last acknowledged value is the one every range must serve).
Issue(id, k, v) ==
    /\ DOMAIN reqs = {}
    /\ id \notin issued
    /\ issued' = issued \cup {id}
    /\ reqs' = [x \in DOMAIN reqs \cup {id} |-> IF x = id THEN <<k, v>> ELSE reqs[x]]
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, acked,
                   applied, epoch0, gEpoch, g2Epoch, stale>>

Answer(id) == reqs' = [x \in DOMAIN reqs \ {id} |-> reqs[x]]

\* The request reaches G (the router does not know G' yet, or the key is not under P). A key
\* under P after SetRangeBounds gets OutOfRange: the request stays and is retried. With `lost`,
\* the answer does not reach the client, which retries with the same id.
ToG(id, lost) ==
    LET k == reqs[id][1] v == reqs[id][2] IN
    /\ id \in DOMAIN reqs
    /\ ~(k \in Under /\ fresh /\ done)
    /\ ~(k \in Under /\ out)
    /\ IF id \in gDedup
       THEN /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
            /\ UNCHANGED <<gKeys, gDedup, applied, stale>>
       ELSE /\ gKeys' = [gKeys EXCEPT ![k] = v]
            /\ gDedup' = gDedup \cup {id}
            /\ applied' = [applied EXCEPT ![id] = @ + 1]
            /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
            /\ stale' = (stale \/ gEpoch /= epoch0)
    /\ IF lost THEN UNCHANGED reqs ELSE Answer(id)
    /\ UNCHANGED <<pending, done, gc, out, snap, g2Keys, g2Dedup, fresh, issued, epoch0, gEpoch,
                   g2Epoch>>

\* The request reaches G' (the router's copy shows it and the key is under P).
ToG2(id, lost) ==
    LET k == reqs[id][1] v == reqs[id][2] IN
    /\ id \in DOMAIN reqs
    /\ k \in Under /\ fresh /\ done
    /\ IF id \in g2Dedup
       THEN /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
            /\ UNCHANGED <<g2Keys, g2Dedup, applied, stale>>
       ELSE /\ g2Keys' = [g2Keys EXCEPT ![k] = v]
            /\ g2Dedup' = g2Dedup \cup {id}
            /\ applied' = [applied EXCEPT ![id] = @ + 1]
            /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
            /\ stale' = (stale \/ g2Epoch /= epoch0)
    /\ IF lost THEN UNCHANGED reqs ELSE Answer(id)
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, fresh, issued, epoch0, gEpoch,
                   g2Epoch>>

SplitIntent ==
    /\ ~pending /\ ~done /\ ~gc
    /\ pending' = TRUE
    /\ UNCHANGED <<done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs, issued, acked,
                   applied, epoch0, gEpoch, g2Epoch, stale>>

GcStart ==
    /\ ~gc /\ ~pending
    /\ gc' = TRUE
    /\ UNCHANGED <<pending, done, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs, issued,
                   acked, applied, epoch0, gEpoch, g2Epoch, stale>>

GcFinish ==
    /\ gc
    /\ gc' = FALSE
    /\ UNCHANGED <<pending, done, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs, issued,
                   acked, applied, epoch0, gEpoch, g2Epoch, stale>>

\* G applies SetRangeBounds at index i: every replica builds G''s complete state at i.
SetBounds ==
    /\ pending /\ ~out
    /\ out' = TRUE
    /\ snap' = [ok |-> TRUE,
                keys |-> [k \in Key |-> IF k \in Under THEN gKeys[k] ELSE None],
                dedup |-> gDedup,
                epoch |-> gEpoch]
    /\ UNCHANGED <<pending, done, gc, gKeys, gDedup, g2Keys, g2Dedup, fresh, reqs, issued, acked,
                   applied, epoch0, gEpoch, g2Epoch, stale>>

Complete ==
    /\ pending /\ out /\ snap.ok
    /\ done' = TRUE /\ pending' = FALSE
    /\ g2Keys' = snap.keys /\ g2Dedup' = snap.dedup /\ g2Epoch' = snap.epoch
    /\ UNCHANGED <<gc, out, gKeys, gDedup, snap, fresh, reqs, issued, acked, applied, epoch0,
                   gEpoch, stale>>

Refresh ==
    /\ fresh' = done
    /\ fresh /= done
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, reqs, issued,
                   acked, applied, epoch0, gEpoch, g2Epoch, stale>>

\* A replica set change: Group 0's epoch moves, then each range's leader brings its range to it.
ChangeReplicas ==
    /\ epoch0 = gEpoch /\ (~done \/ epoch0 = g2Epoch) /\ epoch0 < 2
    /\ epoch0' = epoch0 + 1
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs,
                   issued, acked, applied, gEpoch, g2Epoch, stale>>

EpochG ==
    /\ gEpoch < epoch0
    /\ gEpoch' = epoch0
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs,
                   issued, acked, applied, epoch0, g2Epoch, stale>>

EpochG2 ==
    /\ done /\ g2Epoch < epoch0
    /\ g2Epoch' = epoch0
    /\ UNCHANGED <<pending, done, gc, out, gKeys, gDedup, snap, g2Keys, g2Dedup, fresh, reqs,
                   issued, acked, applied, epoch0, gEpoch, stale>>

\* A write whose range's epoch differs from Group 0's is refused (PartitionMoving) and retried:
\* only matching epochs apply, which the guards below encode.
Next ==
    \/ \E id \in Req, k \in Key, v \in Val : Issue(id, k, v)
    \/ \E id \in DOMAIN reqs, lost \in BOOLEAN :
          \/ (gEpoch = epoch0 /\ ToG(id, lost))
          \/ (g2Epoch = epoch0 /\ ToG2(id, lost))
    \/ SplitIntent \/ GcStart \/ GcFinish \/ SetBounds \/ Complete \/ Refresh
    \/ ChangeReplicas \/ EpochG \/ EpochG2

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in BOOLEAN /\ done \in BOOLEAN /\ gc \in BOOLEAN /\ out \in BOOLEAN
    /\ issued \subseteq Req

\* The value each range serves for its keys is the last acknowledged one (an applied request
\* whose answer was lost may also be what it serves, until the client learns it).
Serves(k) == IF k \in Under /\ out THEN g2Keys[k] ELSE gKeys[k]
NoRefLost ==
    \A k \in Key :
        \/ Serves(k) = acked[k]
        \/ \E id \in DOMAIN reqs : reqs[id] = <<k, Serves(k)>> /\ id \in gDedup \cup g2Dedup
        \/ (k \in Under /\ out /\ ~done)

AtMostOnce == \A id \in Req : applied[id] <= 1

SplitAndGcExclusive == ~(gc /\ pending)

NoStaleWrite == ~stale
=============================================================================
