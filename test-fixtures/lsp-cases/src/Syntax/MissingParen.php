<?php

declare(strict_types=1);

namespace App\Syntax;

// ERROR: Missing closing parenthesis in method declaration.
class MissingParen
{
    public function brokenMethod(string $name
    {
        return;
    }
}

