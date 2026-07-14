"""Metasearch - Privacy-first search with 200+ engines and AI answers.

pip install metasearch

Usage:
    from metasearchgo import Metasearch, BriefingGenerator

    # Search
    ms = Metasearch()
    results = ms.search("AI")
    answer = ms.answer("What is AI?")

    # Briefing
    gen = BriefingGenerator()
    briefing = await gen.create("ko")
"""

from metasearchgo.client import (
    Metasearch,
    SearchResult,
    Answer,
)
from metasearchgo.briefing import (
    BriefingGenerator,
    Briefing,
    VOICES,
)

__version__ = "0.1.0"
__all__ = [
    "Metasearch",
    "SearchResult",
    "Answer",
    "BriefingGenerator",
    "Briefing",
    "VOICES",
]
