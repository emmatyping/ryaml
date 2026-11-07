"""Construct the scalar types which need Python's standard library."""

import binascii
import datetime
import re

from .error import ConstructorError


_TIMESTAMP = re.compile(
    r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})"
    r"(?:(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})"
    r"(?:\.([0-9]*))?"
    r"(?:[ \t]*(Z|([-+])([0-9]{1,2})(?::([0-9]{2}))?))?)?$"
)


def timestamp(value: str) -> datetime.date | datetime.datetime:
    # Explicit tags use this pattern too, including single-digit months/days.
    # Like PyYAML, which calls match.groupdict(), malformed explicitly tagged
    # timestamps raise AttributeError; invalid calendar fields are rejected by
    # datetime with ValueError.
    match = _TIMESTAMP.match(value)
    if match is None:
        raise AttributeError("'NoneType' object has no attribute 'groupdict'")
    year, month, day, hour, minute, second, fraction, zone, sign, zh, zm = match.groups()
    date = (int(year), int(month), int(day))
    if hour is None:
        return datetime.date(*date)
    tz = None
    if sign:
        offset = datetime.timedelta(hours=int(zh), minutes=int(zm or 0))
        tz = datetime.timezone(-offset if sign == "-" else offset)
    elif zone:
        tz = datetime.timezone.utc
    return datetime.datetime(
        *date, int(hour), int(minute), int(second),
        int((fraction or "")[:6].ljust(6, "0")), tzinfo=tz,
    )


def binary(value: str) -> bytes:
    try:
        encoded = value.encode("ascii")
    except UnicodeEncodeError as error:
        raise ConstructorError(f"failed to convert base64 data into ascii: {error}") from error
    try:
        return binascii.a2b_base64(encoded)
    except binascii.Error as error:
        raise ConstructorError(f"failed to decode base64 data: {error}") from error
