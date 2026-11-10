---------------------------- MODULE HlcKeyLatches ----------------------------
(* The reads a connection's thread serves outside the core (005 FR-028) when they look at the     *)
(* writes in flight of their own key only, not of the whole range (H-010 of the native tests:     *)
(* with the count of the range, a third of the reads of YCSB-B went back to the core). Same steps  *)
(* as HlcLatches, over two keys: a write counts its key in flight, then gets a mark above the read  *)
(* cache of its key and above the bound of the outside reads; an outside read raises the bound to   *)
(* its mark, then reads its key if no write of that key is counted, and goes to the core otherwise. *)
(* A write that counts its key only after its mark is the case the design forbids (a key it writes *)
(* without having latched it before): CountBeforeMark = FALSE is that mutant.                      *)
(* ConnChecks, MarkAboveBound and CountBeforeMark turn each defence off: without any of them,      *)
(* ReadsAreStable fails.                                                                            *)
(* Invariant: a read of key k served at t keeps seeing the same version of k.                      *)
EXTENDS Naturals, FiniteSets

CONSTANTS Keys, MaxMark, MaxWrites, MaxReads, ConnChecks, MarkAboveBound, CountBeforeMark

VARIABLES
    floor,      \* the read cache of each key: the highest mark read in the core
    bound,      \* atomic bound of the reads served outside the core (the range's)
    pend,       \* writes counted in flight, by key
    marking,    \* the key of a write that has no mark yet ("none": no such write)
    inflight,   \* <<mark, key>> of the writes whose latch is held (proposed, not applied)
    versions,   \* <<mark, key>> of the applied versions
    reads,      \* served reads: <<key, mark, mark of the version seen (0 for none)>>
    conn,       \* an outside read between its two steps: <<key, mark>>, or <<"none", 0>>
    writes      \* writes begun

vars == <<floor, bound, pend, marking, inflight, versions, reads, conn, writes>>

None == "none"
Marks == 1..MaxMark
Max(s) == IF s = {} THEN 0 ELSE CHOOSE m \in s : \A n \in s : n <= m
Seen(k, t) == Max({v[1] : v \in {x \in versions : x[2] = k /\ x[1] <= t}})

Init ==
    /\ floor = [k \in Keys |-> 0]
    /\ bound = 0
    /\ pend = [k \in Keys |-> 0]
    /\ marking = None
    /\ inflight = {}
    /\ versions = {}
    /\ reads = {}
    /\ conn = <<None, 0>>
    /\ writes = 0

\* A write of key k begins (one marking at a time, at most one in flight per key); with
\* CountBeforeMark it is counted in flight now, before its mark.
Count ==
    /\ writes < MaxWrites
    /\ marking = None
    /\ \E k \in Keys :
        /\ pend[k] = 0
        /\ ~\E x \in inflight : x[2] = k
        /\ marking' = k
        /\ pend' = IF CountBeforeMark THEN [pend EXCEPT ![k] = 1] ELSE pend
    /\ writes' = writes + 1
    /\ UNCHANGED <<floor, bound, inflight, versions, reads, conn>>

\* ...then takes a mark above its key's read cache and the bound.
Propose ==
    /\ marking /= None
    /\ LET k == marking IN
        \E m \in Marks :
            /\ m > floor[k]
            /\ MarkAboveBound => m > bound
            /\ <<m, k>> \notin versions
            /\ inflight' = inflight \cup {<<m, k>>}
    /\ marking' = None
    /\ UNCHANGED <<floor, bound, pend, versions, reads, conn, writes>>

\* The mutant's late count: a write with its mark counts its key only now.
CountLate ==
    /\ \E x \in inflight :
        /\ pend[x[2]] = 0
        /\ pend' = [pend EXCEPT ![x[2]] = 1]
    /\ UNCHANGED <<floor, bound, marking, inflight, versions, reads, conn, writes>>

\* Its entry is applied: the version is visible and the write is no longer in flight.
Apply ==
    /\ \E x \in inflight :
        /\ pend[x[2]] = 1
        /\ versions' = versions \cup {x}
        /\ inflight' = inflight \ {x}
        /\ pend' = [pend EXCEPT ![x[2]] = 0]
    /\ UNCHANGED <<floor, bound, marking, reads, conn, writes>>

\* A read of key k in the core at mark t waits for the latches of k with mark <= t, then enters
\* k's read cache. The core does not run it between the two steps of one of its writes.
Read ==
    /\ Cardinality(reads) < MaxReads
    /\ marking = None
    /\ \E k \in Keys, t \in Marks :
        /\ \A x \in inflight : x[2] = k => x[1] > t
        /\ reads' = reads \cup {<<k, t, Seen(k, t)>>}
        /\ floor' = [floor EXCEPT ![k] = IF t > @ THEN t ELSE @]
    /\ UNCHANGED <<bound, pend, marking, inflight, versions, conn, writes>>

\* A read of key k outside the core raises the bound to its mark...
ConnStart ==
    /\ conn[1] = None
    /\ Cardinality(reads) < MaxReads
    /\ \E k \in Keys, t \in Marks :
        /\ conn' = <<k, t>>
        /\ bound' = IF t > bound THEN t ELSE bound
    /\ UNCHANGED <<floor, pend, marking, inflight, versions, reads, writes>>

\* ...then reads if no write of its key is in flight; otherwise it goes to the core.
ConnFinish ==
    /\ conn[1] /= None
    /\ LET k == conn[1]
           t == conn[2]
       IN reads' = IF pend[k] = 0 \/ ~ConnChecks
                      THEN reads \cup {<<k, t, Seen(k, t)>>}
                      ELSE reads
    /\ conn' = <<None, 0>>
    /\ UNCHANGED <<floor, bound, pend, marking, inflight, versions, writes>>

Next == Count \/ Propose \/ CountLate \/ Apply \/ Read \/ ConnStart \/ ConnFinish

Spec == Init /\ [][Next]_vars

ReadsAreStable == \A r \in reads : Seen(r[1], r[2]) = r[3]

TypeOK ==
    /\ bound \in 0..MaxMark
    /\ \A k \in Keys : floor[k] \in 0..MaxMark /\ pend[k] \in 0..1
    /\ marking \in Keys \cup {None}
    /\ writes \in 0..MaxWrites
=============================================================================
