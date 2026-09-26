"""Fixture: a small, domain-neutral Python file exercising class, method,
free-function, and constant extraction."""

MAX_RETRIES = 3
default_currency = "XYZ"


class Account:
    DEFAULT_BALANCE = 0

    def deposit(self, amount):
        return amount

    def withdraw(self, amount):
        return amount


def build_account():
    return Account()


def max_retries_for(account):
    return MAX_RETRIES
