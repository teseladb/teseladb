---------------------------- MODULE Smoke ----------------------------
(* Trivial model to check that scripts/tlc.sh works (001 FR-015). *)
EXTENDS Naturals

VARIABLE x

Init == x = 0
(* Counts up to 3 and stays there (the last state is not a deadlock). *)
Next == (x < 3 /\ x' = x + 1) \/ (x = 3 /\ UNCHANGED x)
Spec == Init /\ [][Next]_x

TypeOK == x \in 0..3
=======================================================================
