(ns simple.kw-destructure
  "Destructures a keyword defined elsewhere in the project, so its name is
  both a local binding and the key being read."
  (:require [simple.keywords :as kw]))

(defn read-local [{::kw/keys [local]}]
  local)
