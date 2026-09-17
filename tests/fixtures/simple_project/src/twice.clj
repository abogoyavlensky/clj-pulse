(ns simple.twice
  "Two usages of one var on one line: a site set that counts per line, not per
  line-and-column, has to answer both.")

(defn twin [x] x)

(defn both [x]
  (+ (twin x) (twin x)))

;; A map value whose key is spelled `keys`: data, not destructuring.
(def config {::keys [:a :b]})

(defn config-keys []
  (::keys config))

;; An astral character before a token: columns count UTF-16 units.
(defn smile [x] (str "😀" (twin x)))

