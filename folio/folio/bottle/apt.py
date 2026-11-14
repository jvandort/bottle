"""An apt repository, as a feature declares it (see folio-cli's features.rs): what folio-cli installs in an image,
and keeps the packages of (its images.rs).
"""

from dataclasses import dataclass


@dataclass(frozen=True)
class Repository:
    uri: str  # http:// or https://, to the repository's root (where dists/ and pool/ are)
    suite: str = ""  # e.g. stable, or trixie
    components: tuple[str, ...] = ()  # e.g. main
    key: str = ""  # the file of the key it's signed with, in its feature's directory
