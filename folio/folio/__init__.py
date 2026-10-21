"""Folio: threads of work that outlive their sandboxes.

A thread is one line of work in one repo: a bottle to work in, a lane in the
repo for its commits, and the Claude session working on it. Folio drives
bottle through its CLI and never imports it (see engine.py).
"""
