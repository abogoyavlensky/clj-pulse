(ns simple.twice
  "Two usages of one var on one line: a site set that counts per line, not per
  line-and-column, has to answer both.")

(defn twin [x] x)

(defn both [x]
  (+ (twin x) (twin x)))
