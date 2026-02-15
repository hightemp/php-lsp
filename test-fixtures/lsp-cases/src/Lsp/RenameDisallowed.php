<?php

declare(strict_types=1);

namespace App\Lsp;

// CASE: Variable rename is not supported by current server implementation.
function cannotRenameVariable(): void
{
    $counter = 1;
    $counter++;
    echo $counter;
}

// CASE: Built-in symbol rename should be blocked when stubs are loaded.
function cannotRenameBuiltin(): int
{
    return strlen('abc');
}

// CASE: Invalid new name samples for rename request validation:
// - "new name" (contains space) -> invalid params
// - "Bad\\Name" (contains backslash) -> invalid params

