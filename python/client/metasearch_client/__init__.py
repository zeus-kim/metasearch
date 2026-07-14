"""Metasearch Python Client - Search 200+ engines with AI answers."""

import requests
from typing import Optional, List, Dict, Any, Iterator
from dataclasses import dataclass

__version__ = "0.1.0"

@dataclass
class SearchResult:
    title: str
    url: str
    content: str
    engine: str
    score: float = 0.0

@dataclass
class Answer:
    text: str
    citations: List[Dict[str, str]]
    tokens: int = 0

class Metasearch:
    """Metasearch API client."""

    def __init__(self, base_url: str = "http://localhost:8889", timeout: int = 30):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.session = requests.Session()

    def search(
        self,
        query: str,
        categories: Optional[List[str]] = None,
        lang: str = "auto",
        page: int = 1,
        safe_search: int = 1,
    ) -> List[SearchResult]:
        """Search across all engines."""
        params = {
            "q": query,
            "format": "json",
            "language": lang,
            "pageno": page,
            "safesearch": safe_search,
        }
        if categories:
            params["categories"] = ",".join(categories)

        resp = self.session.get(
            f"{self.base_url}/search",
            params=params,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        data = resp.json()

        return [
            SearchResult(
                title=r.get("title", ""),
                url=r.get("url", ""),
                content=r.get("content", ""),
                engine=r.get("engine", ""),
                score=r.get("score", 0.0),
            )
            for r in data.get("results", [])
        ]

    def answer(
        self,
        query: str,
        focus: str = "general",
        model: Optional[str] = None,
        stream: bool = False,
    ) -> Answer:
        """Get AI-grounded answer with citations."""
        params = {"q": query, "focus": focus}
        if model:
            params["model"] = model

        if stream:
            return self._stream_answer(params)

        resp = self.session.get(
            f"{self.base_url}/api/v1/answer",
            params=params,
            timeout=self.timeout * 2,
        )
        resp.raise_for_status()
        data = resp.json()

        return Answer(
            text=data.get("answer", ""),
            citations=data.get("citations", []),
            tokens=data.get("tokens", {}).get("total", 0),
        )

    def _stream_answer(self, params: dict) -> Iterator[str]:
        """Stream answer chunks via SSE."""
        resp = self.session.get(
            f"{self.base_url}/answer",
            params=params,
            stream=True,
            timeout=self.timeout * 2,
        )
        resp.raise_for_status()

        for line in resp.iter_lines(decode_unicode=True):
            if line and line.startswith("data:"):
                yield line[5:].strip()

    def research(
        self,
        query: str,
        focus: str = "general",
        deep: bool = True,
        followups: bool = True,
    ) -> Dict[str, Any]:
        """Deep research with subqueries and followups."""
        payload = {
            "query": query,
            "focus": focus,
            "deep": deep,
            "followups": followups,
        }
        resp = self.session.post(
            f"{self.base_url}/api/v1/research",
            json=payload,
            timeout=self.timeout * 3,
        )
        resp.raise_for_status()
        return resp.json()

    def discover(
        self,
        lang: str = "en",
        category: Optional[str] = None,
        limit: int = 50,
    ) -> List[Dict[str, Any]]:
        """Get curated news feed."""
        params = {"lang": lang, "limit": limit}
        if category:
            params["category"] = category

        resp = self.session.get(
            f"{self.base_url}/api/v1/discover_snapshot",
            params=params,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return resp.json().get("articles", [])

    def trending(self, geo: str = "US") -> List[Dict[str, Any]]:
        """Get trending topics."""
        resp = self.session.get(
            f"{self.base_url}/api/v1/trending",
            params={"geo": geo},
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return resp.json().get("trends", [])

    def images(
        self,
        query: str,
        lang: str = "auto",
        safe_search: int = 1,
    ) -> List[Dict[str, Any]]:
        """Search images."""
        resp = self.session.get(
            f"{self.base_url}/search",
            params={
                "q": query,
                "categories": "images",
                "format": "json",
                "language": lang,
                "safesearch": safe_search,
            },
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return resp.json().get("results", [])

    def news_digest(self, query: str, lang: str = "en") -> Dict[str, Any]:
        """Get AI-summarized news digest."""
        resp = self.session.get(
            f"{self.base_url}/api/v1/news_digest",
            params={"q": query, "lang": lang},
            timeout=self.timeout * 2,
        )
        resp.raise_for_status()
        return resp.json()

    def health(self) -> Dict[str, Any]:
        """Check server health."""
        resp = self.session.get(
            f"{self.base_url}/health",
            timeout=5,
        )
        resp.raise_for_status()
        return resp.json()

    def __repr__(self) -> str:
        return f"Metasearch({self.base_url!r})"
