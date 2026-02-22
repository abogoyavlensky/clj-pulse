(ns my.platform)

#?(:clj  (defn read-file [path] (slurp path))
   :cljs (defn read-file [path] (js/fetch path)))

(defn shared-fn [x] (* x 2))
