------------------------- MODULE SingleServerChange -------------------------
(* Raft membership with single-server changes (001 FR-015).                                  *)
(* A configuration is the voter set of the last configuration entry a server has committed     *)
(* (teseladb-raft applies a change when it hands the entry out, which is once it is committed). A *)
(* leader proposes a change only if none is in flight and an entry of its own term is           *)
(* committed. Any server answers a vote: only the candidate's own                                *)
(* configuration decides which votes count. An election is atomic: a candidate wins with a       *)
(* quorum of its own configuration that may vote for it, and their votes are recorded at once   *)
(* (the votes of a candidate that loses only restrict what can happen, so leaving them out      *)
(* loses no behaviour with two leaders in a term). Servers restart keeping their persistent     *)
(* state (term, vote, log, commit index).                                                       *)
(* Invariant: never two leaders in one term.                                                    *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Server, InitVoters, MaxTerm, MaxLog, Nil

VARIABLES term, votedFor, role, log, commit

vars == <<term, votedFor, role, log, commit>>

\* Entries: a configuration change carries the new voter set; a plain entry carries {}.
Entry == [term : 1..MaxTerm, conf : BOOLEAN, voters : SUBSET Server]

\* Voters of the last configuration entry committed by i (or the initial ones).
Conf(i) ==
    LET idx == {k \in 1..commit[i] : log[i][k].conf}
    IN IF idx = {} THEN InitVoters ELSE log[i][CHOOSE k \in idx : \A m \in idx : m <= k].voters

Majority(S, V) == 2 * Cardinality(S \cap V) > Cardinality(V)

LastTerm(l) == IF Len(l) = 0 THEN 0 ELSE l[Len(l)].term

UpToDate(c, i) ==
    \/ LastTerm(log[c]) > LastTerm(log[i])
    \/ LastTerm(log[c]) = LastTerm(log[i]) /\ Len(log[c]) >= Len(log[i])

Init ==
    /\ term = [i \in Server |-> 0]
    /\ votedFor = [i \in Server |-> Nil]
    /\ role = [i \in Server |-> "Follower"]
    /\ log = [i \in Server |-> <<>>]
    /\ commit = [i \in Server |-> 0]

\* A voter of its own configuration starts an election.
Timeout(i) ==
    /\ i \in Conf(i)
    /\ role[i] /= "Leader"
    /\ term[i] < MaxTerm
    /\ term' = [term EXCEPT ![i] = term[i] + 1]
    /\ votedFor' = [votedFor EXCEPT ![i] = i]
    /\ role' = [role EXCEPT ![i] = "Candidate"]
    /\ UNCHANGED <<log, commit>>

\* Server i may vote for candidate c (any server answers; its own configuration does not matter).
MayVote(i, c) ==
    /\ \/ term[i] < term[c]
       \/ term[i] = term[c] /\ votedFor[i] \in {Nil, c}
    /\ UpToDate(c, i)

\* Candidate c wins with a quorum q of its own configuration; only those votes count.
BecomeLeader(c, q) ==
    /\ role[c] = "Candidate"
    /\ c \in q
    /\ Majority(q, Conf(c))
    /\ \A i \in q \ {c} : MayVote(i, c)
    /\ Len(log[c]) < MaxLog
    /\ term' = [i \in Server |-> IF i \in q THEN term[c] ELSE term[i]]
    /\ votedFor' = [i \in Server |-> IF i \in q THEN c ELSE votedFor[i]]
    /\ role' = [i \in Server |-> IF i = c THEN "Leader" ELSE IF i \in q THEN "Follower" ELSE role[i]]
    \* The term's first entry (the Noop of teseladb-raft).
    /\ log' = [log EXCEPT ![c] = Append(log[c], [term |-> term[c], conf |-> FALSE, voters |-> {}])]
    /\ UNCHANGED commit

OwnTermCommitted(l) == \E k \in 1..commit[l] : log[l][k].term = term[l]

ChangeInFlight(l) == \E k \in (commit[l] + 1)..Len(log[l]) : log[l][k].conf

\* Single-server change: one voter more or one fewer.
ProposeChange(l, s) ==
    /\ role[l] = "Leader"
    /\ Len(log[l]) < MaxLog
    /\ OwnTermCommitted(l)
    /\ ~ChangeInFlight(l)
    /\ LET new == IF s \in Conf(l) THEN Conf(l) \ {s} ELSE Conf(l) \cup {s}
       IN /\ new /= {}
          /\ log' = [log EXCEPT ![l] = Append(log[l], [term |-> term[l], conf |-> TRUE, voters |-> new])]
    /\ UNCHANGED <<term, votedFor, role, commit>>

\* The leader brings i's log to its own (prefixes match after a successful append).
Replicate(l, i) ==
    /\ role[l] = "Leader"
    /\ l /= i
    /\ term[i] <= term[l]
    /\ term' = [term EXCEPT ![i] = term[l]]
    /\ role' = [role EXCEPT ![i] = "Follower"]
    /\ votedFor' = [votedFor EXCEPT ![i] = IF term[i] < term[l] THEN Nil ELSE votedFor[i]]
    /\ log' = [log EXCEPT ![i] = log[l]]
    /\ commit' = [commit EXCEPT ![i] = commit[l]]

Agree(l, n) == {i \in Server : Len(log[i]) >= n /\ SubSeq(log[i], 1, n) = SubSeq(log[l], 1, n)}

\* Only entries of its own term are committed by counting, with its own configuration.
AdvanceCommit(l) ==
    /\ role[l] = "Leader"
    /\ \E n \in (commit[l] + 1)..Len(log[l]) :
        /\ log[l][n].term = term[l]
        /\ Majority(Agree(l, n), Conf(l))
        /\ commit' = [commit EXCEPT ![l] = n]
    /\ UNCHANGED <<term, votedFor, role, log>>

Restart(i) ==
    /\ role[i] /= "Follower"
    /\ role' = [role EXCEPT ![i] = "Follower"]
    /\ UNCHANGED <<term, votedFor, log, commit>>

Next ==
    \/ \E i \in Server : Timeout(i) \/ AdvanceCommit(i) \/ Restart(i)
    \/ \E i \in Server, q \in SUBSET Server : BecomeLeader(i, q)
    \/ \E i, j \in Server : Replicate(i, j) \/ ProposeChange(i, j)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ term \in [Server -> 0..MaxTerm]
    /\ votedFor \in [Server -> Server \cup {Nil}]
    /\ role \in [Server -> {"Follower", "Candidate", "Leader"}]
    /\ \A i \in Server : Len(log[i]) <= MaxLog /\ \A k \in 1..Len(log[i]) : log[i][k] \in Entry
    /\ commit \in [Server -> 0..MaxLog]

\* The servers are interchangeable when every one starts as a voter (TLC's SYMMETRY).
Symmetry == Permutations(Server)

ElectionSafety ==
    \A i, j \in Server : role[i] = "Leader" /\ role[j] = "Leader" /\ term[i] = term[j] => i = j
=============================================================================
