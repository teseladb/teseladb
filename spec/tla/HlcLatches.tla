------------------------------ MODULE HlcLatches ------------------------------
(* Latches with a mark and the reads that wait for them (005 FR-007, T039), on one leader.       *)
(* A write takes the latch of its key, gets a mark above the read cache of the key, and its      *)
(* version becomes visible only when its entry is applied, which frees the latch. A read at      *)
(* mark t waits while a latch with mark <= t is held, then enters the read cache at t and sees   *)
(* the newest version at or below t. Clocks are left out: HlcLease checks how marks are taken.   *)
(* Invariant: a read served at t keeps seeing the same version, so no version appears later at *)
(* or below a mark already read.                                                               *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxMark, MaxWrites, MaxReads

VARIABLES
    floor,      \* read cache of the key: the highest mark read
    inflight,   \* marks of the writes whose latch is held (proposed, not applied)
    versions,   \* marks of the applied versions
    reads,      \* served reads: <<mark, mark of the version seen (0 for none)>>
    writes      \* writes proposed

vars == <<floor, inflight, versions, reads, writes>>

Max(s) == IF s = {} THEN 0 ELSE CHOOSE m \in s : \A n \in s : n <= m
Seen(t) == Max({v \in versions : v <= t})
Marks == 1..MaxMark

Init ==
    /\ floor = 0
    /\ inflight = {}
    /\ versions = {}
    /\ reads = {}
    /\ writes = 0

\* A write takes the key's latch (one at a time) and a mark above the read cache (FR-004).
Propose ==
    /\ writes < MaxWrites
    /\ inflight = {}
    /\ \E m \in Marks :
        /\ m > floor
        /\ m \notin versions
        /\ inflight' = {m}
    /\ writes' = writes + 1
    /\ UNCHANGED <<floor, versions, reads>>

\* Its entry is applied: the version is visible and the latch goes.
Apply ==
    /\ \E m \in inflight :
        /\ versions' = versions \cup {m}
        /\ inflight' = inflight \ {m}
    /\ UNCHANGED <<floor, reads, writes>>

\* A read at mark t waits for the latches with mark <= t (FR-007), then enters the read cache.
Read ==
    /\ Cardinality(reads) < MaxReads
    /\ \E t \in Marks :
        /\ \A m \in inflight : m > t
        /\ reads' = reads \cup {<<t, Seen(t)>>}
        /\ floor' = IF t > floor THEN t ELSE floor
    /\ UNCHANGED <<inflight, versions, writes>>

Next == Propose \/ Apply \/ Read

Spec == Init /\ [][Next]_vars

ReadsAreStable == \A r \in reads : Seen(r[1]) = r[2]

TypeOK ==
    /\ floor \in 0..MaxMark
    /\ inflight \subseteq Marks
    /\ versions \subseteq Marks
    /\ writes \in 0..MaxWrites
=============================================================================
