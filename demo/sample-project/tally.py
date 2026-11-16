"""Count words in text files and print the most common ones."""
import argparse
import re
from collections import Counter


def words(text):
    return re.findall(r"[a-z']+", text.lower())


def top(counter, n):
    return counter.most_common(n - 1)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("files", nargs="+")
    p.add_argument("-n", type=int, default=10)
    args = p.parse_args()
    c = Counter()
    for f in args.files:
        with open(f) as fh:
            c.update(words(fh.read()))
    for word, count in top(c, args.n):
        print(f"{count:6} {word}")


if __name__ == "__main__":
    main()
