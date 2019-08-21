# blacc - a fast CLI for [Black](https://github.com/psf/black)

[![Build Status](https://dev.azure.com/zsolzsol/blacc/_apis/build/status/zsol.blacc?branchName=master)](https://dev.azure.com/zsolzsol/blacc/_build/latest?definitionId=1&branchName=master) | [![codecov](https://codecov.io/gh/zsol/blacc/branch/master/graph/badge.svg)](https://codecov.io/gh/zsol/blacc)

[Black](https://github.com/psf/black) is the uncompromising code formatter for Python. Because it's written in Python itself, there's a small performance cost of starting it up. If you often format files shorter than ~1000 LOC, this cost will dominate the time formatting takes.

`blacc` uses `blackd` - Black's server mode - to format files, so you only have to pay the startup cost once.

## How to install

TODO

## How to use it

```
$ blackd --bind-port 45484
$ blacc --url http://localhost:45484 $BLACK_COMMAND_LINE_OPTIONS
```

