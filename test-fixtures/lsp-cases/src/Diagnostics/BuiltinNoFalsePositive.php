<?php

declare(strict_types=1);

namespace App\Diagnostics;

// CASE: Built-in class should not trigger unknown-class diagnostics.
$dt = new \DateTimeImmutable('now');

// CASE: Built-in functions should not trigger unknown-function diagnostics.
$len = strlen('abc');
$mapped = array_map(static fn(int $x): int => $x + 1, [1, 2, 3]);

// CASE: Built-in scalar and special type names should not trigger unknown-class diagnostics.
class ParentEntity
{
}

class ChildEntity extends ParentEntity
{
    // CASE: "self" and "static" should not be treated as unknown classes.
    public function withSelf(self $arg): static
    {
        return $this;
    }
}

