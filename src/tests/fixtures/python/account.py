"""Fixture: a small, domain-neutral Python file exercising class, method,
free-function, nested-function, enum, and constant extraction."""

import enum

MAX_RETRIES = 3
default_currency = "XYZ"


class AccountStatus(enum.Enum):
    OPEN = "open"
    CLOSED = "closed"


def account_status_for(account):
    return AccountStatus.OPEN


class Account:
    DEFAULT_BALANCE = 0

    def deposit(self, amount):
        return amount

    def withdraw(self, amount):
        return amount


def build_account():
    def _audit(account):
        # a closure longer than the module-level function it shadows, so only
        # being local keeps it second
        checked = account
        checked = checked or Account()
        checked = checked or Account()
        checked = checked or Account()
        return checked

    return _audit(Account())


def _audit(account):
    return account


def max_retries_for(account):
    return MAX_RETRIES
