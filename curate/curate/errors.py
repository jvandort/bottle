"""The one exception the tool raises on purpose."""


class Refused(Exception):
    """Something the tool declines to do. Exit 1, having touched nothing."""
