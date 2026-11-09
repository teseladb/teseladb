------------------------------ MODULE HlcLatches ------------------------------
(* Latches with a mark and the reads that wait for them (005 FR-007, T039), on one leader, and    *)
(* the reads a connection's thread serves outside the core (005 FR-028, T045).                    *)
(* A write takes the latch of its key, gets a mark above the read cache of the key, and its       *)
(* version becomes visible only when its entry is applied, which frees the latch. A read at mark  *)
(* t in the core waits while a latch with mark <= t is held, then enters the read cache at t and  *)
(* sees the newest version at or below t. Clocks are left out: HlcLease checks how marks are      *)
(* taken.                                                                                          *)
(* A read outside the core cannot see the latches: it raises an atomic bound of the reads to its  *)
(* mark first, then reads only if no write of the range is in flight, and goes back to the core    *)
(* otherwise. The core counts a write in flight before it gives the write its mark, and gives it  *)
(* a mark above the bound too; in the core both steps run together, but the outside read may come  *)
(* between them, so the model takes them apart. With sequentially consistent atomics, either the  *)
(* read sees the write in flight or the write sees the read's bound.                               *)
(* ConnChecks and MarkAboveBound turn each defence off: without either, ReadsAreStable fails      *)
(* (09-11-2026: at depth 8 without the check of the writes in flight, at depth 11 without the mark *)
(* above the bound).                                                                               *)
(* Invariant: a read served at t keeps seeing the same version, so no version appears later at    *)
(* or below a mark already read.                                                                  *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxMark, MaxWrites, MaxReads, ConnChecks, MarkAboveBound

VARIABLES
    floor,      \* read cache of the key: the highest mark read in the core
    bound,      \* atomic bound of the reads served outside the core
    pending,    \* writes of the range counted in flight (from before their mark until applied)
    marking,    \* a write counted in flight that has no mark yet
    inflight,   \* marks of the writes whose latch is held (proposed, not applied)
    versions,   \* marks of the applied versions
    reads,      \* served reads: <<mark, mark of the version seen (0 for none)>>
    conn,       \* the mark of a read outside the core between its two steps (0: none)
    writes      \* writes proposed

vars == <<floor, bound, pending, marking, inflight, versions, reads, conn, writes>>

Max(s) == IF s = {} THEN 0 ELSE CHOOSE m \in s : \A n \in s : n <= m
Seen(t) == Max({v \in versions : v <= t})
Marks == 1..MaxMark

Init ==
    /\ floor = 0
    /\ bound = 0
    /\ pending = 0
    /\ marking = FALSE
    /\ inflight = {}
    /\ versions = {}
    /\ reads = {}
    /\ conn = 0
    /\ writes = 0

\* A write of the core is counted in flight (one at a time)...
Count ==
    /\ writes < MaxWrites
    /\ pending = 0
    /\ pending' = 1
    /\ marking' = TRUE
    /\ writes' = writes + 1
    /\ UNCHANGED <<floor, bound, inflight, versions, reads, conn>>

\* ...then takes its key's latch and a mark above the read cache and the bound (FR-004, FR-028).
Propose ==
    /\ marking
    /\ \E m \in Marks :
        /\ m > floor
        /\ MarkAboveBound => m > bound
        /\ m \notin versions
        /\ inflight' = {m}
    /\ marking' = FALSE
    /\ UNCHANGED <<floor, bound, pending, versions, reads, conn, writes>>

\* Its entry is applied: the version is visible, the latch goes and it is no longer in flight.
Apply ==
    /\ \E m \in inflight :
        /\ versions' = versions \cup {m}
        /\ inflight' = inflight \ {m}
    /\ pending' = pending - 1
    /\ UNCHANGED <<floor, bound, marking, reads, conn, writes>>

\* A read in the core at mark t waits for the latches with mark <= t (FR-007), then enters the read
\* cache. The core does not run it between the two steps of one of its writes.
Read ==
    /\ Cardinality(reads) < MaxReads
    /\ ~marking
    /\ \E t \in Marks :
        /\ \A m \in inflight : m > t
        /\ reads' = reads \cup {<<t, Seen(t)>>}
        /\ floor' = IF t > floor THEN t ELSE floor
    /\ UNCHANGED <<bound, pending, marking, inflight, versions, conn, writes>>

\* A read outside the core raises the bound to its mark...
ConnStart ==
    /\ conn = 0
    /\ Cardinality(reads) < MaxReads
    /\ \E t \in Marks :
        /\ conn' = t
        /\ bound' = IF t > bound THEN t ELSE bound
    /\ UNCHANGED <<floor, pending, marking, inflight, versions, reads, writes>>

\* ...then reads if no write is in flight; otherwise it goes to the core (and is not served here).
ConnFinish ==
    /\ conn /= 0
    /\ reads' = IF pending = 0 \/ ~ConnChecks THEN reads \cup {<<conn, Seen(conn)>>} ELSE reads
    /\ conn' = 0
    /\ UNCHANGED <<floor, bound, pending, marking, inflight, versions, writes>>

Next == Count \/ Propose \/ Apply \/ Read \/ ConnStart \/ ConnFinish

Spec == Init /\ [][Next]_vars

ReadsAreStable == \A r \in reads : Seen(r[1]) = r[2]

TypeOK ==
    /\ floor \in 0..MaxMark
    /\ bound \in 0..MaxMark
    /\ pending \in 0..1
    /\ inflight \subseteq Marks
    /\ versions \subseteq Marks
    /\ conn \in 0..MaxMark
    /\ writes \in 0..MaxWrites
=============================================================================
