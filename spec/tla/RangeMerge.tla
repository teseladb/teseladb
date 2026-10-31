------------------------------- MODULE RangeMerge -------------------------------
(* Merging the right range R into its left neighbour L (007 FR-009, FR-022).                      *)
(* Group 0 records the intent (MergeRanges) only when both ranges have the same replicas. R then  *)
(* applies MergeFreeze at index j: from j it refuses every request (the router retries) and each *)
(* of its replicas keeps R's state at j. L applies Merge: a replica of L takes R's frozen state  *)
(* from the replica of R on its own node, so L's replicas must be the nodes that froze R. Until   *)
(* Merge applies, L's log may give the merge up instead (its replicas differ, or not every one of *)
(* them has R frozen in time): no Merge of that intent applies afterwards, Group 0 aborts and R   *)
(* unfreezes. After Merge, L serves R's keys with the union of both dedup tables and the greater  *)
(* last timestamp. Group 0 completes the merge (R's descriptor goes) and routers refresh: a stale *)
(* one sends R's keys to R, which refuses them, or to L before Merge, which answers OutOfRange.   *)
(* Answers can be lost and the client retries with the same request id.                           *)
(* Invariants: no acknowledged value is lost, no request is applied twice, L never merges a state *)
(* that one of its replicas does not hold, and a key's timestamps grow in the range that serves  *)
(* it.                                                                                            *)
EXTENDS Naturals, FiniteSets

CONSTANTS KeyL, KeyR, Req, Val, Node, MaxTs

Key == KeyL \cup KeyR
None == 0

VARIABLES
    phase,      \* Group 0: "idle" | "intent" | "done"; an abort goes back to "idle"
    frozen,     \* R applied MergeFreeze and has not unfrozen
    frozenSt,   \* R's state at j: [keys, dedup, last, rep]
    merged,     \* L applied Merge
    mergeRep,   \* ghost: L's replicas when it applied Merge
    fresh,      \* the router's copy of Group 0 shows the merge
    lKeys, lDedup, lLast, lRep,
    rKeys, rDedup, rLast, rRep,
    reqs, issued, acked, applied,
    order       \* ghost: FALSE once a key got a stamp not above its previous one

vars == <<phase, frozen, frozenSt, merged, mergeRep, fresh, lKeys, lDedup, lLast, lRep, rKeys,
          rDedup, rLast, rRep, reqs, issued, acked, applied, order>>

NoFrozen == [keys |-> [k \in Key |-> <<None, 0>>], dedup |-> {}, last |-> 0, rep |-> {}]

Init ==
    /\ phase = "idle" /\ frozen = FALSE /\ frozenSt = NoFrozen
    /\ merged = FALSE /\ mergeRep = {} /\ fresh = FALSE
    /\ lKeys = [k \in Key |-> <<None, 0>>] /\ lDedup = {} /\ lLast = 0 /\ lRep = Node
    /\ rKeys = [k \in Key |-> <<None, 0>>] /\ rDedup = {} /\ rLast = 0 /\ rRep = Node
    /\ reqs = [id \in {} |-> <<>>] /\ issued = {}
    /\ acked = [k \in Key |-> None]
    /\ applied = [id \in Req |-> 0]
    /\ order = TRUE

Issue(id, k, v) ==
    /\ DOMAIN reqs = {}
    /\ id \notin issued
    /\ issued' = issued \cup {id}
    /\ reqs' = [x \in {id} |-> <<k, v>>]
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, lRep,
                   rKeys, rDedup, rLast, rRep, acked, applied, order>>

Answer(id, lost) == IF lost THEN UNCHANGED reqs ELSE reqs' = [x \in DOMAIN reqs \ {id} |-> reqs[x]]

\* L serves its own keys, and R's once it applied Merge and the router knows it.
ToL(id, lost) ==
    LET k == reqs[id][1] v == reqs[id][2] IN
    /\ id \in DOMAIN reqs
    /\ k \in KeyL \/ (merged /\ fresh)
    /\ lLast < MaxTs
    /\ IF id \in lDedup
       THEN UNCHANGED <<lKeys, lDedup, lLast, applied, order>>
       ELSE /\ lKeys' = [lKeys EXCEPT ![k] = <<v, lLast + 1>>]
            /\ lLast' = lLast + 1
            /\ lDedup' = lDedup \cup {id}
            /\ applied' = [applied EXCEPT ![id] = @ + 1]
            /\ order' = (order /\ lKeys[k][2] < lLast + 1)
    /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
    /\ Answer(id, lost)
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, fresh, lRep, rKeys, rDedup, rLast,
                   rRep, issued>>

\* R serves its keys while it is not frozen and the router does not know of the merge.
ToR(id, lost) ==
    LET k == reqs[id][1] v == reqs[id][2] IN
    /\ id \in DOMAIN reqs
    /\ k \in KeyR /\ ~(merged /\ fresh) /\ ~frozen /\ ~merged
    /\ rLast < MaxTs
    /\ IF id \in rDedup
       THEN UNCHANGED <<rKeys, rDedup, rLast, applied, order>>
       ELSE /\ rKeys' = [rKeys EXCEPT ![k] = <<v, rLast + 1>>]
            /\ rLast' = rLast + 1
            /\ rDedup' = rDedup \cup {id}
            /\ applied' = [applied EXCEPT ![id] = @ + 1]
            /\ order' = (order /\ rKeys[k][2] < rLast + 1)
    /\ acked' = IF lost THEN acked ELSE [acked EXCEPT ![k] = v]
    /\ Answer(id, lost)
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, lRep,
                   rRep, issued>>

\* A replica change in either range: one node leaves it or comes back.
ChangeL(n) ==
    /\ lRep' = IF n \in lRep THEN lRep \ {n} ELSE lRep \cup {n}
    /\ lRep' /= {}
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, rKeys,
                   rDedup, rLast, rRep, reqs, issued, acked, applied, order>>

ChangeR(n) ==
    /\ ~frozen /\ ~merged
    /\ rRep' = IF n \in rRep THEN rRep \ {n} ELSE rRep \cup {n}
    /\ rRep' /= {}
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, lRep,
                   rKeys, rDedup, rLast, reqs, issued, acked, applied, order>>

\* Group 0 records the intent only with the same replicas on both sides.
Intent ==
    /\ phase = "idle" /\ ~merged /\ lRep = rRep
    /\ phase' = "intent"
    /\ UNCHANGED <<mergeRep, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, lRep, rKeys,
                   rDedup, rLast, rRep, reqs, issued, acked, applied, order>>

Freeze ==
    /\ phase = "intent" /\ ~frozen
    /\ frozen' = TRUE
    /\ frozenSt' = [keys |-> rKeys, dedup |-> rDedup, last |-> rLast, rep |-> rRep]
    /\ UNCHANGED <<mergeRep, phase, merged, fresh, lKeys, lDedup, lLast, lRep, rKeys, rDedup, rLast,
                   rRep, reqs, issued, acked, applied, order>>

Max(a, b) == IF a > b THEN a ELSE b

\* L applies Merge with R's frozen state, which each of its replicas reads on its own node.
Merge ==
    /\ phase = "intent" /\ frozen /\ ~merged
    /\ lRep = frozenSt.rep
    /\ merged' = TRUE /\ mergeRep' = lRep
    /\ lKeys' = [k \in Key |-> IF k \in KeyR THEN frozenSt.keys[k] ELSE lKeys[k]]
    /\ lDedup' = lDedup \cup frozenSt.dedup
    /\ lLast' = Max(lLast, frozenSt.last)
    /\ UNCHANGED <<phase, frozen, frozenSt, fresh, lRep, rKeys, rDedup, rLast, rRep, reqs, issued,
                   acked, applied, order>>

\* L's log gives the merge up before Merge applies: Group 0 aborts and R unfreezes.
Abort ==
    /\ phase = "intent" /\ ~merged
    /\ phase' = "idle" /\ frozen' = FALSE /\ frozenSt' = NoFrozen
    /\ UNCHANGED <<mergeRep, merged, fresh, lKeys, lDedup, lLast, lRep, rKeys, rDedup, rLast, rRep,
                   reqs, issued, acked, applied, order>>

Complete ==
    /\ phase = "intent" /\ merged
    /\ phase' = "done"
    /\ UNCHANGED <<mergeRep, frozen, frozenSt, merged, fresh, lKeys, lDedup, lLast, lRep, rKeys,
                   rDedup, rLast, rRep, reqs, issued, acked, applied, order>>

Refresh ==
    /\ fresh /= (phase = "done")
    /\ fresh' = (phase = "done")
    /\ UNCHANGED <<mergeRep, phase, frozen, frozenSt, merged, lKeys, lDedup, lLast, lRep, rKeys,
                   rDedup, rLast, rRep, reqs, issued, acked, applied, order>>

Next ==
    \/ \E id \in Req, k \in Key, v \in Val : Issue(id, k, v)
    \/ \E id \in DOMAIN reqs, lost \in BOOLEAN : ToL(id, lost) \/ ToR(id, lost)
    \/ \E n \in Node : ChangeL(n) \/ ChangeR(n)
    \/ Intent \/ Freeze \/ Merge \/ Abort \/ Complete \/ Refresh

Spec == Init /\ [][Next]_vars

TypeOK == phase \in {"idle", "intent", "done"} /\ issued \subseteq Req

\* The value served for k: L's for its own keys and for R's once merged; R's frozen state between
\* the freeze and Merge (nothing can change it); R's otherwise.
Served(k) ==
    IF k \in KeyL \/ merged THEN lKeys[k][1]
    ELSE IF frozen THEN frozenSt.keys[k][1] ELSE rKeys[k][1]
Dedup == IF merged THEN lDedup ELSE lDedup \cup rDedup

NoAckedLost ==
    \A k \in Key :
        \/ Served(k) = acked[k]
        \/ \E id \in DOMAIN reqs : reqs[id] = <<k, Served(k)>> /\ id \in Dedup
AtMostOnce == \A id \in Req : applied[id] <= 1
MergeOnlyHeld == merged => mergeRep = frozenSt.rep
StampsGrow == order
=============================================================================
