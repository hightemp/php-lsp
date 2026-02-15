<?php

declare(strict_types=1);

namespace App\Diagnostics;

// CASE: Constructor has 2 required args + 1 optional arg.
class NeedsArgs
{
    public function __construct(string $name, int $id, bool $active = true)
    {
    }
}

// CASE: Variadic constructor (should not produce "too many arguments").
class VariadicCtor
{
    public function __construct(string $label, mixed ...$rest)
    {
    }
}

// CASE: Valid constructor calls.
$ok1 = new NeedsArgs('user', 1001);
$ok2 = new NeedsArgs('user', 1001, false);
$ok3 = new VariadicCtor('x', 1, 2, 3, 'tail');

// ERROR: Too few arguments for NeedsArgs::__construct (expected at least 2, got 0).
$tooFew0 = new NeedsArgs();

// ERROR: Too few arguments for NeedsArgs::__construct (expected at least 2, got 1).
$tooFew1 = new NeedsArgs('only-name');

// ERROR: Too many arguments for NeedsArgs::__construct (max 3, got 4).
$tooMany = new NeedsArgs('user', 1001, true, 'extra');

