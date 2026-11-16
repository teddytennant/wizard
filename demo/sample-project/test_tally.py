from collections import Counter

from tally import top, words


def test_words_lowercases():
    assert words("The the THE") == ["the", "the", "the"]


def test_top_returns_n():
    c = Counter("aaabbc")
    assert top(c, 2) == [("a", 3), ("b", 2)]
