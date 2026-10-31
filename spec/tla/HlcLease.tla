------------------------------ MODULE HlcLease ------------------------------
(* HLC marks, the read cache and the lease of a range (005 FR-003 to FR-006, FR-025; D-003, D-004). *)
(* Time is global and discrete: one tick is the 125 ms the clock rule allows, so a node's physical  *)
(* clock is now + off[n], with off[n] in -1..1 while NTP keeps it in time (two nodes differ by at   *)
(* most 2 ticks, 250 ms). A clock can jump 3 ticks (400 ms) either way, one node at a time (the     *)
(* fault three voters tolerate), until NTP steps it back. The local rule (FR-006) lets a node take  *)
(* the lease, write and read only while its offset from the median clock of the voters, its own     *)
(* included, is at most one tick, and not within Window ticks (1 s) of a jump of its wall clock     *)
(* against its monotonic one, which it sees at once.                                                *)
(* A mark is <<t, n>>: its physical part and the node that assigned it, compared in that order; the *)
(* sequence within a millisecond is folded into t (succ moves to the next mark the node can give). *)
(* The node that takes the lease starts its read cache at the lowest mark of the tick after the one *)
(* a lease above the clock it heard, and at least 2 ticks above its wall clock, as the code does    *)
(* (read_cache_minimum, raise_to_read_cache and TsCache::for_lease: the millisecond after the       *)
(* lease), and raises its clock to it before its lease rounds go out. While it holds the lease its  *)
(* clock stays at least at its wall clock (keep_lease_clock), and each round its voters acknowledge *)
(* (Renew, sent also while it stands aside) carries that clock to them and extends the lease: they  *)
(* hand it to the next holder (005 FR-003, FR-005, research R12). They learn it without loss: a     *)
(* node sends nothing while its bound on disk is below a mark it received (research R13). A write   *)
(* takes a mark above its node's clock, the group's last entry and the read cache, and may be lost  *)
(* before it commits (its mark stays in the holder's clock only); a read with the lease is served   *)
(* at the holder's clock, at the highest mark of its tick, and raises the cache. With Back > 0 the  *)
(* clocks of a majority can also go back or forward Back ticks together (more than the margin):     *)
(* they stay within the margin of each other, so the median follows them. Each node keeps its peak, *)
(* the highest wall clock it read while in margin, and stays aside while its clock is below it      *)
(* (FR-006). When a majority falls, the peak rule and the raise of the clock cover each other       *)
(* (D-083): with the clock the voters carry, either one keeps the next holder's read cache above    *)
(* every read served. When a majority moves ahead and comes back, only the raise covers it: the     *)
(* holder read at its wall clock ahead, and a node that never read so far ahead takes the next      *)
(* lease below those reads unless the rounds carried that clock (seed 730 of range-docs).           *)
(* SingleJumps turns the jumps of one node on or off: the two kinds of fault are checked with       *)
(* separate configurations, so that each finishes in minutes, and MaxJumps bounds the jumps of      *)
(* either kind in a behaviour.                                                                      *)
(* Invariants: no two versions share a mark, and a read served at t keeps seeing the same version: *)
(* no version appears later at or below t.                                                         *)
EXTENDS Integers, FiniteSets, Sequences

CONSTANTS N, MaxTime, LeaseTicks, MaxWrites, MaxReads, Window, Back, SingleJumps, MaxJumps

Node == 1..N

VARIABLES
    now,        \* global time
    off,        \* each node's clock offset in ticks (-1..1, or 3 ticks away after a jump)
    aside,      \* global time until which each node stays aside after a jump of its clock
    peak,       \* highest wall clock each node read while in margin
    hlc,        \* each node's last mark
    holder,     \* node with the lease (0: none)
    leaseEnd,   \* global time the lease ends
    floor,      \* read cache of the holder: no write at or below it
    carried,    \* clock the voters of the holder's lease heard from it, which the next holder learns
    last,       \* mark of the group's last entry
    versions,   \* marks of the written versions
    reads,      \* served reads: <<read mark, mark of the version seen (<<0, 0>> for none)>>
    writes,     \* writes made (two with one mark would leave fewer versions)
    lost,       \* whether a write was lost (at most one)
    jumps       \* clock jumps so far (of one node or of a majority), at most MaxJumps

vars == <<now, off, aside, peak, hlc, holder, leaseEnd, floor, carried, last, versions, reads, writes, lost, jumps>>

Zero == <<0, 0>>
Less(a, b) == a[1] < b[1] \/ (a[1] = b[1] /\ a[2] < b[2])
Max2(a, b) == IF Less(a, b) THEN b ELSE a
\* The smallest mark above m that node n can assign (FR-025).
Succ(m, n) == IF m[2] < n THEN <<m[1], n>> ELSE <<m[1] + 1, n>>
Phys(n) == now + off[n]
\* The median of the voters' clock offsets (N is odd).
Median == CHOOSE o \in {off[n] : n \in Node} :
              /\ Cardinality({n \in Node : off[n] < o}) <= N \div 2
              /\ Cardinality({n \in Node : off[n] > o}) <= N \div 2
\* The local clock rule (FR-006) at global time t: within one tick of the median, past the window
\* of a jump, and not below the peak.
InMarginAt(n, t) == off[n] - Median \in -1..1 /\ aside[n] < t /\ t + off[n] >= peak[n]
InMargin(n) == InMarginAt(n, now)
Jumped(n) == off[n] \notin -1..1
\* The newest version at or below mark t.
Seen(t) == LET below == {v \in versions : ~Less(t, v)}
           IN IF below = {} THEN Zero ELSE CHOOSE v \in below : \A w \in below : ~Less(v, w)

Init ==
    /\ now = 4
    /\ off \in [Node -> -1..1]
    /\ aside = [n \in Node |-> 0]
    /\ peak = [n \in Node |-> now + off[n]]
    /\ hlc = [n \in Node |-> Zero]
    /\ holder = 0
    /\ leaseEnd = 0
    /\ floor = Zero
    /\ carried = Zero
    /\ last = Zero
    /\ versions = {}
    /\ reads = {}
    /\ writes = 0
    /\ lost = FALSE
    /\ jumps = 0

\* Each node in margin at the new time keeps its wall clock as its peak.
Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ peak' = [n \in Node |-> IF InMarginAt(n, now + 1) /\ now + 1 + off[n] > peak[n]
                               THEN now + 1 + off[n] ELSE peak[n]]
    /\ UNCHANGED <<off, aside, hlc, holder, leaseEnd, floor, carried, last, versions, reads, writes, lost, jumps>>

\* A node takes the lease once the previous one ended. Its read cache starts at the lowest mark of the
\* tick after the one a lease above the clock it heard (its own, the group's last entry and what its
\* voters carried from the previous holder), and at least 2 ticks above its wall clock (FR-005,
\* research R12; <<t, 0>> is below every mark a node gives in tick t). The previous holder read until
\* its lease ended, a lease after the last round its voters acknowledged (Renew), at its clock then:
\* in the tick a lease above the clock that round carried at the latest, at any mark of that tick (in
\* the code, the millisecond a lease above it: both clocks are read in whole milliseconds). Its clock
\* goes to the cache's minimum, and the rounds of its lease carry that clock to its voters. Raft
\* gives it the group's last entry.
Acquire(n) ==
    /\ InMargin(n)
    /\ holder = 0 \/ leaseEnd < now
    /\ LET h == Max2(Max2(hlc[n], last), carried)
           f == Max2(<<h[1] + LeaseTicks + 1, 0>>, <<Phys(n) + 2, 0>>)
       IN /\ hlc' = [hlc EXCEPT ![n] = f]
          /\ floor' = f
          /\ carried' = f
    /\ holder' = n
    /\ leaseEnd' = now + LeaseTicks
    /\ UNCHANGED <<now, off, aside, peak, last, versions, reads, writes, lost, jumps>>

\* A round of the holder's lease that a majority acknowledges: the lease lasts LeaseTicks from it.
\* While it leads, the holder keeps its clock at least at its wall clock (keep_lease_clock), and the
\* round carries that clock to the voters, which hand it to the next holder (005 FR-005, research R12).
\* Raft sends the rounds whether or not the holder is in margin: a holder whose clock jumped keeps its
\* lease while it stays aside, and its rounds carry the clock it jumped to.
Renew ==
    /\ holder /= 0 /\ now <= leaseEnd
    /\ leaseEnd < now + LeaseTicks
    /\ LET c == Max2(hlc[holder], <<Phys(holder), 0>>)
       IN /\ hlc' = [hlc EXCEPT ![holder] = c]
          /\ carried' = Max2(carried, c)
    /\ leaseEnd' = now + LeaseTicks
    /\ UNCHANGED <<now, off, aside, peak, holder, floor, last, versions, reads, writes, lost, jumps>>

\* A write by the holder: above its clock, the last entry and the read cache (FR-004). It commits,
\* or it is lost with its holder before any voter hears of it: its mark stays in the holder's clock.
Write ==
    /\ holder /= 0 /\ now <= leaseEnd /\ InMargin(holder)
    /\ writes < MaxWrites
    /\ writes' = writes + 1
    /\ LET n == holder
           m == Max2(Succ(Max2(Max2(hlc[n], last), floor), n), <<Phys(n), n>>)
       IN /\ hlc' = [hlc EXCEPT ![n] = m]
          /\ \/ /\ versions' = versions \cup {m}
                /\ last' = m
                /\ UNCHANGED lost
             \/ /\ ~lost
                /\ lost' = TRUE
                /\ UNCHANGED <<versions, last>>
    /\ UNCHANGED <<now, off, aside, peak, holder, leaseEnd, floor, carried, reads, jumps>>

\* A read with the lease, served at the holder's clock (its HLC, or its wall clock if ahead), at
\* the highest mark of that tick; the cache rises to it.
Read ==
    /\ holder /= 0 /\ now <= leaseEnd /\ InMargin(holder)
    /\ Cardinality(reads) < MaxReads
    /\ LET t == Max2(<<hlc[holder][1], N>>, <<Phys(holder), N>>)
       IN /\ reads' = reads \cup {<<t, Seen(t)>>}
          /\ floor' = Max2(floor, t)
    /\ UNCHANGED <<now, off, aside, peak, hlc, holder, leaseEnd, carried, last, versions, writes, lost, jumps>>

\* A node's wall clock jumps 3 ticks (400 ms) while every other clock is in time.
Jump(n) ==
    /\ SingleJumps
    /\ jumps < MaxJumps
    /\ jumps' = jumps + 1
    /\ \A m \in Node : ~Jumped(m)
    /\ \E d \in {-3, 3} : off' = [off EXCEPT ![n] = off[n] + d]
    /\ aside' = [aside EXCEPT ![n] = now + Window]
    /\ UNCHANGED <<now, peak, hlc, holder, leaseEnd, floor, carried, last, versions, reads, writes, lost>>

\* The wall clocks of a majority go back or forward Back ticks at once (a bad NTP source, say): they
\* jumped, and stay within the margin of each other, so the median follows them.
MajorityBack ==
    /\ Back > 0
    /\ jumps < MaxJumps
    /\ jumps' = jumps + 1
    /\ \A m \in Node : ~Jumped(m)
    /\ \E S \in SUBSET Node, d \in {-Back, Back} :
        /\ Cardinality(S) = N \div 2 + 1
        /\ off' = [n \in Node |-> IF n \in S THEN off[n] + d ELSE off[n]]
        /\ aside' = [n \in Node |-> IF n \in S THEN now + Window ELSE aside[n]]
    /\ UNCHANGED <<now, peak, hlc, holder, leaseEnd, floor, carried, last, versions, reads, writes, lost>>

\* NTP steps the jumped clock back into time.
Resync(n) ==
    /\ Jumped(n)
    /\ \E o \in -1..1 : off' = [off EXCEPT ![n] = o]
    /\ aside' = [aside EXCEPT ![n] = now + Window]
    /\ UNCHANGED <<now, peak, hlc, holder, leaseEnd, floor, carried, last, versions, reads, writes, lost, jumps>>

Next == Tick \/ Write \/ Read \/ Renew \/ MajorityBack \/ \E n \in Node : Acquire(n) \/ Jump(n) \/ Resync(n)

Spec == Init /\ [][Next]_vars

\* What TLC tells states apart by: a node's aside time and the lease end only matter while they are
\* ahead (every earlier value behaves the same from now on), and once the lease ended no action
\* reads its holder or its read cache until the next one takes the lease.
View == LET over == leaseEnd < now
        IN <<now, off, [n \in Node |-> IF aside[n] < now THEN 0 ELSE aside[n]], peak, hlc,
             IF over THEN 0 ELSE holder, IF over THEN 0 ELSE leaseEnd, IF over THEN Zero ELSE floor,
             carried, last, versions, reads, writes, lost, jumps>>

\* Every write that commits leaves its own version: no two writes share a mark (FR-025, D-003).
UniqueMarks == Cardinality(versions) + (IF lost THEN 1 ELSE 0) = writes

ReadsAreStable == \A r \in reads : Seen(r[1]) = r[2]

TypeOK ==
    /\ now \in 4..MaxTime
    /\ off \in [Node -> -(Back + 4)..(Back + 4)]
    /\ holder \in 0..N
    /\ writes \in 0..MaxWrites
=============================================================================
