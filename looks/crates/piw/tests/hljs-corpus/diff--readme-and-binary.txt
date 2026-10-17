diff --git a/README.md b/README.md
index 4a1d0b2..c97e3f5 100644
--- a/README.md
+++ b/README.md
@@ -3,10 +3,12 @@
 A small tool for converting CSV to Parquet.
 
 ## Install
 
-    pip install csv2pq
+    pip install csv2pq[fast]
 
 ## Usage
 
-    csv2pq in.csv out.parquet
+    csv2pq in.csv out.parquet --compression zstd
 
+Supports UTF-8 headers such as `名前,年齢,città` out of the box.
 See `csv2pq --help` for all options.
diff --git a/assets/logo.png b/assets/logo.png
index 8d3c1aa..f0e2b94 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
diff --git a/old_name.txt b/new_name.txt
similarity index 92%
rename from old_name.txt
rename to new_name.txt
index 5e6f7a8..9b0c1d2 100644
--- a/old_name.txt
+++ b/new_name.txt
@@ -1,3 +1,3 @@
 line one (été)
-line two
+line 2 🙂
 line three
