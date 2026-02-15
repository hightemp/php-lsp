<?php

declare(strict_types=1);

namespace App\Syntax;

// ERROR: Missing expression after assignment.
$value = ;

// ERROR: Incomplete function declaration.
function incomplete(

// ERROR: Missing class closing brace.
class MissingBrace
{
    public string $name = 'x';

